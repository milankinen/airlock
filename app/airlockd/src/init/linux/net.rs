//! Guest networking: bring loopback up and give it `10.0.0.1/32` so
//! the in-VM DNS server can listen there. All other egress runs
//! through the TCP proxy on the TUN (see `net::tcp_proxy`), which
//! installs its own default route on `airlock0`. Host-published ports
//! are handled by per-port loopback listeners (see
//! `net::host_port_forward`), so no iptables rules are required.

use std::process::Command;

use tracing::{debug, info};

/// Configure loopback networking. The default route is installed by
/// `tcp_proxy::start` once `airlock0` is up.
pub(super) fn setup(_host_ports: &[u16]) -> anyhow::Result<()> {
    run_cmd(&["/sbin/ip", "link", "set", "lo", "up"])?;

    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1")?;
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1")?;

    // Only the /32 — if we reserved the whole /8 for lo, the virtual-DNS
    // IPs we hand out (10.2.0.0/16) would shadow the default airlock0
    // route and sink to lo with no listener.
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

fn write_sysctl(path: &str, value: &str) -> anyhow::Result<()> {
    std::fs::write(path, value).map_err(|e| anyhow::anyhow!("sysctl {path}={value} failed: {e}"))
}

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
