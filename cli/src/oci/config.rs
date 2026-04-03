use std::path::Path;

use oci_client::config::ConfigFile;

use crate::vm::mounts::ContainerBind;

/// Generate an OCI runtime spec config.json from the image config and bind mounts.
pub fn generate_config(
    image_config: &ConfigFile,
    project_cwd: &Path,
    binds: &[ContainerBind],
    user_args: &[String],
    terminal: bool,
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
    let mut mounts = vec![
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

    // Bind mounts from VirtioFS shares into container
    for bind in binds {
        let mut options = vec!["bind".to_string()];
        if bind.read_only {
            options.push("ro".to_string());
        }
        mounts.push(serde_json::json!({
            "destination": bind.destination,
            "type": "bind",
            "source": bind.source,
            "options": options
        }));
    }

    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "process": {
            "terminal": terminal,
            "user": { "uid": uid, "gid": gid },
            "args": args,
            "env": env,
            "cwd": cwd
        },
        "root": {
            "path": "rootfs",
            "readonly": false
        },
        "hostname": "ezpez",
        "mounts": mounts,
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
