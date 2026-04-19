//! Guest networking: bring loopback up, give it a routable /8 so the
//! in-VM proxy can listen on 10.0.0.1, and redirect all outbound TCP
//! through the proxy on port 15001. Also writes `/etc/resolv.conf`
//! pointing at the proxy's DNS listener.

use std::process::Command;

use tracing::{debug, info};

/// Configure loopback networking and iptables rules.
pub(super) fn setup(host_ports: &[u16]) -> anyhow::Result<()> {
    run_cmd(&["/sbin/ip", "link", "set", "lo", "up"])?;

    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1")?;
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1")?;

    run_cmd(&["/sbin/ip", "addr", "add", "10.0.0.1/8", "dev", "lo"])?;
    run_cmd(&[
        "/sbin/ip", "route", "add", "default", "via", "10.0.0.1", "dev", "lo",
    ])?;

    for port in host_ports {
        run_cmd(&[
            "/usr/sbin/iptables",
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "127.0.0.1",
            "--dport",
            &port.to_string(),
            "-j",
            "REDIRECT",
            "--to-port",
            "15001",
        ])?;
    }
    run_cmd(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "-d",
        "127.0.0.1",
        "-j",
        "RETURN",
    ])?;
    run_cmd(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "--dport",
        "15001",
        "-j",
        "RETURN",
    ])?;
    run_cmd(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "-j",
        "REDIRECT",
        "--to-port",
        "15001",
    ])?;

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
