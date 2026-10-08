mod test_file_backends;
mod test_secrets;
mod test_subst;
mod test_vault_lock;

use std::collections::HashMap;
use std::path::Path;

use super::*;
use crate::test_cfg::vault::FixedPassphrase;

fn file_vault(path: &Path) -> Vault {
    Vault::new_with(
        Box::new(FileStorage::new(path.to_path_buf())),
        HashMap::new(),
        VaultStorageType::File,
    )
}

fn encrypted_vault(path: &Path, passphrase: &'static str) -> Vault {
    Vault::new_with(
        Box::new(EncryptedFileStorage::new(
            path.to_path_buf(),
            Box::new(FixedPassphrase(passphrase)),
        )),
        HashMap::new(),
        VaultStorageType::EncryptedFile,
    )
}
