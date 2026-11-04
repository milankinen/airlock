//! The `[env]` check of `airlock start`.
//!
//! Finds `[env]` errors before the slow steps start.

use super::Exit;
use crate::config::config_values::ConfigValues;
use crate::vault::Vault;

/// Resolve the `[env]` section of the run config, to find errors early.
/// Args:
///  - `config`: Resolved config values
///  - `vault`: Vault for secret values
///
/// Returns:
///   A configuration error (exit code 2) if a variable is missing, or an
///   abort (exit code 130) if the user aborts the vault passphrase prompt.
// The resolution does the host substitution and makes surrogates for masked
// entries. A missing variable thus fails before the image pull.
pub fn check_env_early(config: &ConfigValues, vault: &Vault) -> Result<(), Exit> {
    crate::sandbox::resolve_env(config, vault)
        .map(drop)
        .map_err(Exit::from)
}

#[cfg(test)]
mod tests {
    //! Tests of the early `[env]` check.

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Context;

    use super::*;
    use crate::cli::prompt::{PromptAborted, PromptError};
    use crate::test_cfg::vault::FixedPassphrase;
    use crate::test_cfg::{resolve_project_toml, temp_dir};
    use crate::util::VerifiedPath;
    use crate::vault::{EncryptedFileStorage, PassphraseSource, VaultStorageType};

    /// Test that the early env check fails only when an env variable is
    /// missing. A missing variable must fail before the slow steps.
    ///   1. Check that the agent packs pass the check
    ///   2. Check that an `[env]` entry and a preset that read unset host
    ///      variables fail with exit code 2
    ///   3. Check that the error names the missing variable
    #[test]
    fn early_env_check_fails_only_on_missing_env_variables() {
        let vault = Vault::for_storage_type(VaultStorageType::Disabled);
        let toml = "[packs]\nclaude = { version = 1 }\ncodex = { version = 1 }\n";
        let values = resolve_project_toml(toml).unwrap().values;
        check_env_early(&values, &vault).unwrap();
        for (toml, missing) in [
            (
                "[packs]\nclaude = { version = 1 }\n[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n",
                "MINE",
            ),
            ("presets = [\"claude-code\"]\n", "CLAUDE_CODE_OAUTH_TOKEN"),
        ] {
            let values = resolve_project_toml(toml).unwrap().values;
            assert!(matches!(
                check_env_early(&values, &vault),
                Err(Exit::Code(2))
            ));
            let Err(e) = crate::sandbox::resolve_env(&values, &vault) else {
                panic!("{missing} resolves");
            };
            assert_eq!(e.name, missing);
        }
    }

    /// A passphrase source whose prompt the user aborts. Counts the
    /// prompts.
    struct AbortedPassphrase {
        abort: fn() -> anyhow::Error,
        prompts: Arc<AtomicUsize>,
    }

    impl PassphraseSource for AbortedPassphrase {
        fn unlock(&self) -> anyhow::Result<String> {
            self.prompts.fetch_add(1, Ordering::Relaxed);
            Err((self.abort)())
        }
        fn create(&self) -> anyhow::Result<String> {
            self.unlock()
        }
    }

    /// Test that an abort of the vault passphrase prompt (Esc or Ctrl+C)
    /// during the early env check exits with code 130, not with the
    /// config error code 2. One abort must end the unlock: the prompt
    /// must not show again for the next secret.
    ///   1. Make an encrypted vault with two secrets
    ///   2. Open it with a passphrase source that the user aborts
    ///   3. Check that `[env]` entries that need the secrets give 130 for
    ///      Esc and for Ctrl+C, with one optional and one required entry
    ///   4. Check that the prompt showed only one time
    #[test]
    fn aborted_vault_passphrase_prompt_exits_with_abort_code() {
        let tmp = temp_dir();
        let path = VerifiedPath::new_follow(tmp.path().join("vault.json"), None).unwrap();
        let vault = |passphrase: Box<dyn PassphraseSource>| {
            Vault::new_with(
                Box::new(EncryptedFileStorage::new(path.clone(), passphrase)),
                HashMap::new(),
                VaultStorageType::EncryptedFile,
            )
        };
        let setup = vault(Box::new(FixedPassphrase("pw".into())));
        setup.set_secret("TOKEN", "abc").unwrap();
        setup.set_secret("OTHER", "def").unwrap();
        let toml = "[packs]\nclaude = { version = 1 }\n[env]\n\
                    A = { value = \"${OTHER}\", optional = true }\nB = \"${TOKEN}\"\n";
        let values = resolve_project_toml(toml).unwrap().values;

        let esc: fn() -> anyhow::Error = || PromptAborted("passphrase".into()).into();
        let ctrl_c: fn() -> anyhow::Error = || {
            Err::<(), _>(PromptError::Interrupted)
                .context("read")
                .unwrap_err()
        };
        for abort in [esc, ctrl_c] {
            let prompts = Arc::new(AtomicUsize::new(0));
            let vault = vault(Box::new(AbortedPassphrase {
                abort,
                prompts: prompts.clone(),
            }));
            assert!(matches!(
                check_env_early(&values, &vault),
                Err(Exit::Code(130))
            ));
            assert_eq!(prompts.load(Ordering::Relaxed), 1);
        }
    }
}
