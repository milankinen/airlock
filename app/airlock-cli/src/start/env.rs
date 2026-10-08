//! The `[env]` check of `airlock start`.

use super::Exit;
use crate::config::config_values::ConfigValues;
use crate::vault::Vault;

/// Resolve `[env]` of the run config (host substitution + surrogates for
/// masked entries), so a missing variable fails before the image pull.
/// That is a configuration error (exit code 2).
pub fn check_env_early(config: &ConfigValues, vault: &Vault) -> Result<(), Exit> {
    crate::project::resolve_env(config, vault)
        .map(drop)
        .map_err(Exit::config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::resolve_project_toml;
    use crate::vault::VaultStorageType;

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
