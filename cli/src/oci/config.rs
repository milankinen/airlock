use oci_client::config::ConfigFile;
use std::path::Path;

/// Generate an OCI runtime spec config.json from the image config.
/// Derives process args, env, workdir, and user from the image.
pub fn generate_config(image_config: &ConfigFile, dest: &Path) -> anyhow::Result<()> {
    let cfg = image_config.config.as_ref();

    // Process args: ENTRYPOINT + CMD, fallback to /bin/sh
    let mut args: Vec<String> = Vec::new();
    if let Some(ep) = cfg.and_then(|c| c.entrypoint.as_ref()) {
        args.extend(ep.iter().cloned());
    }
    if let Some(cmd) = cfg.and_then(|c| c.cmd.as_ref()) {
        args.extend(cmd.iter().cloned());
    }
    if args.is_empty() {
        args.push("/bin/sh".to_string());
    }

    // Environment
    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "TERM=linux".to_string(),
        "HOME=/root".to_string(),
    ];
    if let Some(image_env) = cfg.and_then(|c| c.env.as_ref()) {
        for e in image_env {
            // Override defaults if image sets the same var
            let key = e.split('=').next().unwrap_or("");
            env.retain(|existing| !existing.starts_with(&format!("{key}=")));
            env.push(e.clone());
        }
    }

    let cwd = cfg
        .and_then(|c| c.working_dir.as_deref())
        .unwrap_or("/");

    let user = cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0");
    let (uid, gid) = parse_user(user);

    let config = serde_json::json!({
        "ociVersion": "1.0.0",
        "process": {
            "terminal": true,
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
        "mounts": [
            { "destination": "/proc", "type": "proc", "source": "proc" },
            { "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
              "options": ["nosuid", "strictatime", "mode=755", "size=65536k"] },
            { "destination": "/dev/pts", "type": "devpts", "source": "devpts",
              "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"] },
            { "destination": "/dev/shm", "type": "tmpfs", "source": "shm",
              "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"] },
            { "destination": "/sys", "type": "sysfs", "source": "sysfs",
              "options": ["nosuid", "noexec", "nodev", "ro"] }
        ],
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
