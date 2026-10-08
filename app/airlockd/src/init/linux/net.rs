//! Guest network setup at boot.
//!
//! Sets up loopback networking and the DNS configuration of the container.
//! Only loopback is set up here. The outgoing TCP proxy handles all other
//! traffic, and loopback listeners serve the host-published ports. Thus no
//! firewall rules are necessary.

use std::process::Command;

use tracing::{debug, info};

/// Configure loopback networking. Gives loopback the address `10.0.0.1/32`,
/// where the in-VM DNS server listens.
///
/// `tcp_proxy::start` adds the default route when `airlock0` is up.
pub(super) fn setup(_host_ports: &[u16]) -> anyhow::Result<()> {
    run_cmd(&["/sbin/ip", "link", "set", "lo", "up"])?;

    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1")?;
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1")?;

    // Only the /32. If lo had the full /8, the virtual DNS IPs
    // (10.2.0.0/16) would hide the default airlock0 route. Their traffic
    // would then go to lo, where nothing listens.
    run_cmd(&["/sbin/ip", "addr", "add", "10.0.0.1/32", "dev", "lo"])?;

    info!("networking configured");
    Ok(())
}

/// Point the container's `/etc/resolv.conf` at the in-VM DNS server.
pub(super) fn setup_dns() -> anyhow::Result<()> {
    let dir = "/mnt/overlay/rootfs/etc";
    std::fs::create_dir_all(dir)?;
    std::fs::write(format!("{dir}/resolv.conf"), "nameserver 10.0.0.1\n")?;
    Ok(())
}

/// Write a sysctl value to its `/proc/sys` path.
fn write_sysctl(path: &str, value: &str) -> anyhow::Result<()> {
    std::fs::write(path, value).map_err(|e| anyhow::anyhow!("sysctl {path}={value} failed: {e}"))
}

/// Run a command. The first item of `args` is the program. Returns an error
/// with stderr if the command fails.
fn run_cmd(args: &[&str]) -> anyhow::Result<()> {
    let cmd_str = args.join(" ");
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| anyhow::anyhow!("{cmd_str}: exec failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd_str}: {}", stderr.trim());
    }
    debug!("{cmd_str}: ok");
    Ok(())
}
