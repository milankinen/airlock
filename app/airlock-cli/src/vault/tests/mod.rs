//! Tests for the vault: secrets, registry logins, file backends, the
//! cross-process lock and the substitution of `${NAME}` templates.

mod test_file_backends;
mod test_secrets;
mod test_subst;
mod test_vault_lock;

use std::collections::HashMap;
use std::path::Path;

use super::*;
use crate::test_cfg::vault::FixedPassphrase;

/// The verified form of the test path `path`.
fn verified(path: &Path) -> VerifiedPath {
    VerifiedPath::new_follow(path, None).unwrap()
}

/// A vault that keeps its data in the plaintext file `path`.
fn file_vault(path: &Path) -> Vault {
    Vault::new_with(
        Box::new(FileStorage::new(verified(path))),
        HashMap::new(),
        VaultStorageType::File,
    )
}

/// A vault that keeps its data in the encrypted file `path`, with a fixed
/// passphrase.
fn encrypted_vault(path: &Path, passphrase: &'static str) -> Vault {
    Vault::new_with(
        Box::new(EncryptedFileStorage::new(
            verified(path),
            Box::new(FixedPassphrase(passphrase)),
        )),
        HashMap::new(),
        VaultStorageType::EncryptedFile,
    )
}
