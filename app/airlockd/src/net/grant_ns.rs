//! Network namespaces for temporary network grants.
//!
//! A command that gets temporary network access runs in its own network
//! namespace. A virtual cable connects the namespace to the main network
//! of the VM. Thus the outgoing TCP proxy sees the connections of the
//! command with a source address that is unique for the grant. The proxy
//! sends these connections to the host through the grant's own network
//! proxy, not through the default one.
//!
//! DNS works the same as in the main network: the namespace sends all
//! traffic, also DNS, to the main network of the VM.
//!
//! The main network refuses local packets that use a grant address as
//! source, so other sandbox processes cannot use the grant.

// Test builds of the admin code do not make grant namespaces. Only the
// route lookup and the unit tests below use this module there.
#![cfg_attr(test, allow(dead_code))]

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

use airlock_common::network_capnp::network_proxy;
use tracing::{debug, warn};

/// Number of grant slots. Each slot uses one `/31` of the grant subnet.
const SLOTS: u8 = 128;

/// First three octets of the grant subnet `192.168.78.0/24`.
const SUBNET: [u8; 3] = [192, 168, 78];

/// Shared map from grant source address to the network proxy of the grant.
/// Also keeps the slot pool of the grant addresses.
///
/// The TCP proxy reads it for each new connection. Clones share the same
/// map.
#[derive(Clone, Default)]
pub struct GrantRoutes(Rc<RefCell<Inner>>);

/// State behind [`GrantRoutes`].
#[derive(Default)]
struct Inner {
    /// Grant source address to the proxy of the grant.
    routes: HashMap<Ipv4Addr, network_proxy::Client>,
    /// Slots in use. A slot whose cable could not be deleted stays here,
    /// so no new grant gets its name or address.
    used: BTreeSet<u8>,
    /// Slots that have the firewall rule against spoofed grant addresses.
    /// The rules stay for the life of the VM.
    blocked: BTreeSet<u8>,
}

impl GrantRoutes {
    /// Make an empty route map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Find the proxy for connections from `src`.
    ///
    /// Returns:
    ///   The grant proxy, or `None` if `src` is not a grant address.
    pub(crate) fn lookup(&self, src: Ipv4Addr) -> Option<network_proxy::Client> {
        self.0.borrow().routes.get(&src).cloned()
    }

    /// Take the lowest free slot. Returns `None` if all slots are in use.
    fn alloc(&self) -> Option<u8> {
        let mut inner = self.0.borrow_mut();
        let slot = (0..SLOTS).find(|s| !inner.used.contains(s))?;
        inner.used.insert(slot);
        Some(slot)
    }

    /// Remove the route of `slot`. If `free`, also give back the slot.
    /// Else the slot stays reserved (quarantined).
    fn release(&self, slot: u8, free: bool) {
        let mut inner = self.0.borrow_mut();
        inner.routes.remove(&slot_addrs(slot).grant);
        if free {
            inner.used.remove(&slot);
        }
    }

    /// Make sure that the main network refuses local packets with the
    /// grant address of `slot` as source.
    ///
    /// Processes in the main network do not have the grant address, but an
    /// unprivileged process can still send from it (`IP_FREEBIND`). Its TCP
    /// connections would then go to the grant proxy. Only locally made
    /// packets go through the `OUTPUT` chain. Packets that the grant
    /// namespace sends through the cable are forwarded, so they do not go
    /// through it. The rule covers only the grant side address. The main
    /// network side address of the cable stays usable, for example for
    /// ICMP errors to the grant.
    ///
    /// The rule is a firewall rule and not a policy routing rule, because
    /// the VM kernel has no policy routing.
    fn block_spoofing(&self, slot: u8) -> anyhow::Result<()> {
        if self.0.borrow().blocked.contains(&slot) {
            return Ok(());
        }
        let from = format!("{}/32", slot_addrs(slot).grant);
        let rule = ["OUTPUT", "-s", from.as_str(), "-j", "DROP"];
        // A rule from an earlier try is also good. `-C` fails when the rule
        // does not exist.
        if run_iptables("-C", &rule).is_err() {
            run_iptables("-I", &rule).map_err(|e| e.context("add anti-spoofing rule"))?;
        }
        self.0.borrow_mut().blocked.insert(slot);
        Ok(())
    }

    /// Send the connections from `src` to `proxy`.
    fn register(&self, src: Ipv4Addr, proxy: network_proxy::Client) {
        self.0.borrow_mut().routes.insert(src, proxy);
    }
}

/// Addresses and interface name of one slot.
#[derive(Debug, PartialEq, Eq)]
struct SlotAddrs {
    /// Address of the main network side of the cable.
    root: Ipv4Addr,
    /// Address of the grant namespace side. Also the source address of the
    /// grant connections.
    grant: Ipv4Addr,
    /// Interface name of the main network side.
    ifname: String,
}

/// Get the addresses of `slot`. Slot `n` uses `192.168.78.<2n>/31` on the
/// main network side and `192.168.78.<2n+1>/31` in the namespace.
fn slot_addrs(slot: u8) -> SlotAddrs {
    let base = slot * 2;
    let [a, b, c] = SUBNET;
    SlotAddrs {
        root: Ipv4Addr::new(a, b, c, base),
        grant: Ipv4Addr::new(a, b, c, base + 1),
        ifname: format!("airlockg{slot}"),
    }
}

/// Network namespace of one grant.
///
/// Drop removes the route, deletes the cable and closes the namespace.
/// Processes that still run in the namespace lose their network.
pub struct GrantNet {
    routes: GrantRoutes,
    slot: u8,
    ns: OwnedFd,
}

impl GrantNet {
    /// Make a new network namespace and send its TCP connections to `proxy`.
    /// Args:
    ///  - `routes`: Route map that the TCP proxy uses
    ///  - `proxy`: Network proxy of the grant
    ///
    /// Returns:
    ///   The grant namespace, or error if no slot is free or the setup
    ///   fails.
    pub async fn create(
        routes: &GrantRoutes,
        proxy: network_proxy::Client,
    ) -> anyhow::Result<Self> {
        let slot = routes
            .alloc()
            .ok_or_else(|| anyhow::anyhow!("too many active network grants"))?;
        let addrs = slot_addrs(slot);
        if let Err(e) = routes.block_spoofing(slot) {
            routes.release(slot, true);
            return Err(e.context("grant network setup"));
        }
        // The setup runs `ip` and changes the namespace of the calling
        // thread. Thus it runs in its own thread, and the runtime thread
        // stays free and in the main namespace.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let thread_addrs = slot_addrs(slot);
        let spawned = std::thread::Builder::new()
            .name("grant-netns".into())
            .spawn(move || {
                let _ = tx.send(setup_netns(&thread_addrs));
            });
        let result = match spawned {
            Ok(_) => rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("netns setup thread failed"))),
            Err(e) => Err(e.into()),
        };
        match result {
            Ok(ns) => {
                routes.register(addrs.grant, proxy);
                debug!("grant netns up: {} slot={slot}", addrs.grant);
                Ok(Self {
                    routes: routes.clone(),
                    slot,
                    ns,
                })
            }
            Err(e) => {
                // A failed setup can leave the cable in the main network.
                routes.release(slot, remove_link(&addrs.ifname));
                Err(e.context("grant network setup"))
            }
        }
    }

    /// Namespace FD for `setns`.
    pub fn netns_fd(&self) -> BorrowedFd<'_> {
        self.ns.as_fd()
    }

    /// Source address of the grant connections.
    pub fn addr(&self) -> Ipv4Addr {
        slot_addrs(self.slot).grant
    }
}

impl Drop for GrantNet {
    fn drop(&mut self) {
        let addrs = slot_addrs(self.slot);
        // Deleting the cable is the guarantee that processes that are
        // still in the namespace lose the network. Both ends go away
        // together. This runs `ip` on the runtime thread on purpose (a few
        // ms): the slot must stay reserved until the cable is gone, so a
        // new grant cannot get a name or address that is still in use.
        // A quarantined slot loses its route here, but processes in the old
        // namespace keep the cable. Their connections then go to the
        // default proxy, which applies the normal policy.
        let removed = remove_link(&addrs.ifname);
        self.routes.release(self.slot, removed);
        debug!("grant netns down: {} removed={removed}", addrs.grant);
    }
}

/// Make the namespace and the cable. Runs in a helper thread that exits
/// after the setup.
///
/// Returns:
///   FD of the new network namespace.
#[cfg(target_os = "linux")]
fn setup_netns(addrs: &SlotAddrs) -> anyhow::Result<OwnedFd> {
    use anyhow::Context as _;

    let root_ns =
        OwnedFd::from(std::fs::File::open("/proc/thread-self/ns/net").context("open main netns")?);
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(std::io::Error::last_os_error()).context("unshare(CLONE_NEWNET)");
    }
    let grant_ns =
        OwnedFd::from(std::fs::File::open("/proc/thread-self/ns/net").context("open grant netns")?);

    // Child processes and `/proc/sys/net` follow the namespace of this
    // thread. Thus `ip` and the sysctl writes below apply to the grant
    // namespace.
    let root = addrs.root.to_string();
    let grant_cidr = format!("{}/31", addrs.grant);
    run_ip(&["link", "set", "lo", "up"])?;
    run_ip(&[
        "link",
        "add",
        "eth0",
        "type",
        "veth",
        "peer",
        "name",
        &addrs.ifname,
    ])?;
    // PID 1 is airlockd. Its main thread stays in the main namespace.
    let moved = run_ip(&["link", "set", &addrs.ifname, "netns", "1"]);
    let in_ns = moved.and_then(|()| {
        run_ip(&["addr", "add", &grant_cidr, "dev", "eth0"])?;
        run_ip(&["link", "set", "eth0", "up"])?;
        // All traffic goes to the main network, also DNS to 10.0.0.1.
        // Do not put 10.0.0.1 on lo here.
        run_ip(&["route", "add", "default", "via", &root])?;
        write_sysctl("all/rp_filter", "0");
        write_sysctl("eth0/rp_filter", "0");
        Ok(())
    });
    // On error, the cable is still in the grant namespace (or partly
    // moved). Dropping the namespace FD deletes it with the namespace.
    in_ns?;

    if unsafe {
        libc::setns(
            std::os::fd::AsRawFd::as_raw_fd(&root_ns),
            libc::CLONE_NEWNET,
        )
    } != 0
    {
        let err = std::io::Error::last_os_error();
        // This thread cannot go back. Delete the cable from the grant side.
        let _ = run_ip(&["link", "del", "eth0"]);
        return Err(err).context("setns(main netns)");
    }
    let root_side = (|| {
        let root_cidr = format!("{root}/31");
        run_ip(&["addr", "add", &root_cidr, "dev", &addrs.ifname])?;
        // Replies from the TCP proxy and the DNS server to the grant
        // address use the connected /31 route of this interface. Strict
        // reverse-path filter: packets from the grant must have the grant
        // address of this cable as source. Else a process in one grant
        // could send with the address of another grant (`IP_FREEBIND`) and
        // use its proxy. `all/rp_filter` is 0, so this value applies.
        write_sysctl(&format!("{}/rp_filter", addrs.ifname), "1");
        run_ip(&["link", "set", &addrs.ifname, "up"])
    })();
    if let Err(e) = root_side {
        let _ = run_ip(&["link", "del", &addrs.ifname]);
        return Err(e);
    }
    Ok(grant_ns)
}

/// Non-Linux stub. Always fails.
#[cfg(not(target_os = "linux"))]
fn setup_netns(_addrs: &SlotAddrs) -> anyhow::Result<OwnedFd> {
    anyhow::bail!("network namespaces need Linux")
}

/// Write an IPv4 interface sysctl under `/proc/sys/net/ipv4/conf/` in the
/// namespace of the calling thread. Logs a failure, but does not stop.
#[cfg(target_os = "linux")]
fn write_sysctl(key: &str, value: &str) {
    let path = format!("/proc/sys/net/ipv4/conf/{key}");
    if let Err(e) = std::fs::write(&path, value) {
        warn!("sysctl {path}={value}: {e}");
    }
}

/// Delete the cable `ifname` in the main network, if it is there.
///
/// Returns:
///   `true` if the cable is gone. `false` if it is still there. Then the
///   slot must stay reserved.
fn remove_link(ifname: &str) -> bool {
    let del = run_ip(&["link", "del", ifname]);
    match link_exists(ifname) {
        Ok(false) => true,
        Ok(true) => {
            if let Err(e) = del {
                warn!("grant netns teardown, slot stays reserved: {e:#}");
            }
            false
        }
        Err(e) => {
            warn!("grant netns teardown, slot stays reserved: {e:#}");
            false
        }
    }
}

/// Check if the interface `ifname` exists in the main network.
///
/// Returns:
///   `Ok(false)` only if `ip` ran and reported that the device does not
///   exist. Error if `ip` could not run or failed in an other way.
fn link_exists(ifname: &str) -> anyhow::Result<bool> {
    let out = std::process::Command::new("/sbin/ip")
        .args(["link", "show", "dev", ifname])
        .output()?;
    if out.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("does not exist") {
        return Ok(false);
    }
    anyhow::bail!("ip link show dev {ifname}: {}", stderr.trim())
}

/// Run `iptables` with `op` and `rule` in the main network. `-w` waits for
/// the lock of an other `iptables` run. Returns an error with stderr if it
/// fails.
fn run_iptables(op: &str, rule: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("/usr/sbin/iptables")
        .arg("-w")
        .arg(op)
        .args(rule)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "iptables {op} {}: {}",
            rule.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run `/sbin/ip` with `args` in the namespace of the calling thread.
/// Returns an error with stderr if it fails.
fn run_ip(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("/sbin/ip").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "ip {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the slot pool gives the lowest free slot, refuses when
    /// full, gives a freed slot again and keeps a quarantined slot.
    ///   1. Take all slots and check that they come in order
    ///   2. Check that one more take fails
    ///   3. Free a slot in the middle and check that the next take gets it
    ///   4. Quarantine a slot and check that no take gets it
    #[test]
    fn slot_pool_reuses_released_slot_and_refuses_when_full() {
        let routes = GrantRoutes::new();
        for want in 0..SLOTS {
            assert_eq!(routes.alloc(), Some(want));
        }
        assert_eq!(routes.alloc(), None);
        routes.release(7, true);
        assert_eq!(routes.alloc(), Some(7));
        routes.release(9, false);
        assert_eq!(routes.alloc(), None);
    }

    /// Test that each slot gets its own /31 pair and interface name, and
    /// that the last slot stays in the subnet.
    ///   1. Get the addresses of the first and the last slot
    ///   2. Check the pair addresses and names
    #[test]
    fn slot_addresses_are_distinct_point_to_point_pairs() {
        assert_eq!(
            slot_addrs(0),
            SlotAddrs {
                root: Ipv4Addr::new(192, 168, 78, 0),
                grant: Ipv4Addr::new(192, 168, 78, 1),
                ifname: "airlockg0".into(),
            }
        );
        let last = slot_addrs(SLOTS - 1);
        assert_eq!(last.root, Ipv4Addr::new(192, 168, 78, 254));
        assert_eq!(last.grant, Ipv4Addr::new(192, 168, 78, 255));
        assert!(last.ifname.len() <= 15);
    }
}
