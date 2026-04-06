use std::path::Path;

use super::{OciConfig, ResolvedMount};

/// Generate an OCI runtime spec config.json from the image config and bind mounts.
pub fn generate_config(
    image_config: &OciConfig,
    project_cwd: &Path,
    mounts: &[ResolvedMount],
    user_args: &[String],
    terminal: Option<(u16, u16)>,
    dest: &Path,
) -> anyhow::Result<()> {
    let cfg = image_config.config.as_ref();

    let args: Vec<String> = if user_args.is_empty() {
        let mut a = Vec::new();
        if let Some(ep) = cfg.and_then(|c| c.entrypoint.as_ref()) {
            a.extend(ep.iter().cloned());
        }
        if let Some(cmd) = cfg.and_then(|c| c.cmd.as_ref()) {
            a.extend(cmd.iter().cloned());
        }
        if a.is_empty() {
            a.push("/bin/sh".to_string());
        }
        a
    } else {
        // User args override the entire command
        user_args.to_vec()
    };

    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "TERM=linux".to_string(),
        "HOME=/root".to_string(),
    ];
    if let Some(image_env) = cfg.and_then(|c| c.env.as_ref()) {
        for e in image_env {
            let key = e.split('=').next().unwrap_or("");
            env.retain(|existing| !existing.starts_with(&format!("{key}=")));
            env.push(e.clone());
        }
    }

    let cwd = project_cwd.to_string_lossy().to_string();

    let user = cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0");
    let (uid, gid) = parse_user(user);

    // Build mounts: system mounts first, then bind mounts
    let mut mounts_json = vec![
        serde_json::json!({ "destination": "/proc", "type": "proc", "source": "proc" }),
        serde_json::json!({ "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
          "options": ["nosuid", "strictatime", "mode=755", "size=65536k"] }),
        serde_json::json!({ "destination": "/dev/pts", "type": "devpts", "source": "devpts",
          "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"] }),
        serde_json::json!({ "destination": "/dev/shm", "type": "tmpfs", "source": "shm",
          "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"] }),
        serde_json::json!({ "destination": "/sys", "type": "sysfs", "source": "sysfs",
          "options": ["nosuid", "noexec", "nodev", "ro"] }),
    ];

    // Bind mounts from VirtioFS shares into container.
    // File mounts are excluded — they are overlaid onto the rootfs by init.
    for mount in mounts {
        if matches!(mount.mount_type, super::MountType::File { .. }) {
            continue;
        }
        let mut options = vec!["bind".to_string()];
        if mount.read_only {
            options.push("ro".to_string());
        }
        mounts_json.push(serde_json::json!({
            "destination": mount.target,
            "type": "bind",
            "source": mount.vm_path(),
            "options": options
        }));
    }

    // All Linux capabilities — VM is the security boundary, not crun
    let all_caps = serde_json::json!([
        "CAP_AUDIT_CONTROL",
        "CAP_AUDIT_READ",
        "CAP_AUDIT_WRITE",
        "CAP_BLOCK_SUSPEND",
        "CAP_BPF",
        "CAP_CHECKPOINT_RESTORE",
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_DAC_READ_SEARCH",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_IPC_LOCK",
        "CAP_IPC_OWNER",
        "CAP_KILL",
        "CAP_LEASE",
        "CAP_LINUX_IMMUTABLE",
        "CAP_MAC_ADMIN",
        "CAP_MAC_OVERRIDE",
        "CAP_MKNOD",
        "CAP_NET_ADMIN",
        "CAP_NET_BIND_SERVICE",
        "CAP_NET_BROADCAST",
        "CAP_NET_RAW",
        "CAP_PERFMON",
        "CAP_SETFCAP",
        "CAP_SETGID",
        "CAP_SETPCAP",
        "CAP_SETUID",
        "CAP_SYS_ADMIN",
        "CAP_SYS_BOOT",
        "CAP_SYS_CHROOT",
        "CAP_SYS_MODULE",
        "CAP_SYS_NICE",
        "CAP_SYS_PACCT",
        "CAP_SYS_PTRACE",
        "CAP_SYS_RAWIO",
        "CAP_SYS_RESOURCE",
        "CAP_SYS_TIME",
        "CAP_SYS_TTY_CONFIG",
        "CAP_SYSLOG",
        "CAP_WAKE_ALARM"
    ]);

    let mut process = serde_json::json!({
        "terminal": terminal.is_some(),
        "user": { "uid": uid, "gid": gid },
        "args": args,
        "env": env,
        "cwd": cwd,
        "capabilities": {
            "bounding": all_caps,
            "effective": all_caps,
            "permitted": all_caps,
            "ambient": all_caps,
        }
    });
    if let Some((rows, cols)) = terminal {
        process["consoleSize"] = serde_json::json!({
            "height": rows,
            "width": cols
        });
    }

    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "process": process,
        "root": {
            "path": "rootfs",
            "readonly": false
        },
        "hostname": "ezpez",
        "mounts": mounts_json,
        "linux": {
            "namespaces": [
                { "type": "pid" },
                { "type": "uts" },
                { "type": "mount" }
            ]
        }
    });

    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(dest, json)?;
    Ok(())
}

fn parse_user(user: &str) -> (u32, u32) {
    let parts: Vec<&str> = user.split(':').collect();
    let uid = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let gid = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (uid, gid)
}

/// Get the container uid from the image config.
pub fn get_uid(image_config: &OciConfig) -> u32 {
    let cfg = image_config.config.as_ref();
    let user = cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0");
    parse_user(user).0
}
