use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Config;

pub struct Project {
    pub dir: PathBuf,
    pub hash: String,
    pub cwd: PathBuf,
    pub config: Config,
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

pub fn ensure(config: Config) -> anyhow::Result<Project> {
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    let hash = hex::encode(&hasher.finalize()[..16]);

    let dir = crate::oci::cache::project_dir(&hash)?;
    std::fs::create_dir_all(&dir)?;

    let ca_dir = dir.join("ca");
    let ca_cert = ca_dir.join("ca.crt");
    let ca_key = ca_dir.join("ca.key");

    if !ca_cert.exists() || !ca_key.exists() {
        std::fs::create_dir_all(&ca_dir)?;
        generate_ca(&ca_cert, &ca_key)?;
    }

    Ok(Project {
        dir,
        hash,
        cwd,
        config,
        ca_cert,
        ca_key,
    })
}

fn generate_ca(cert_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let mut params = CertificateParams::new(vec![])?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "ezpez CA");

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    eprintln!("  generated project CA certificate");
    Ok(())
}
