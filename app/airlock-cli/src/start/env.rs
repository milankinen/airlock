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
///   A configuration error (exit code 2) if a variable is missing.
// The resolution does the host substitution and makes surrogates for masked
// entries. A missing variable thus fails before the image pull.
pub fn check_env_early(config: &ConfigValues, vault: &Vault) -> Result<(), Exit> {
    crate::project::resolve_env(config, vault)
        .map(drop)
        .map_err(Exit::config)
}

#[cfg(test)]
mod tests {
    //! Tests of the early `[env]` check.

    use super::*;
    use crate::test_cfg::resolve_project_toml;
    use crate::vault::VaultStorageType;

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
            let Err(e) = crate::project::resolve_env(&values, &vault) else {
                panic!("{missing} resolves");
            };
            assert_eq!(e.name, missing);
        }
    }
}
