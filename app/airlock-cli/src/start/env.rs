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
    use crate::test_support::resolve_project_toml;

    fn vault() -> Vault {
        Vault::new_with(
            Box::new(crate::vault::DisabledStorage),
            std::collections::HashMap::new(),
            crate::vault::VaultStorageType::Disabled,
        )
    }

    /// The claude and codex packs need no `[env]` credential (their
    /// services sign in through the proxy): the check passes. A missing
    /// variable of the user's own `[env]` still fails, and so does the
    /// token of the list form, which is plain config.
    #[test]
    fn early_env_check_fails_only_on_missing_env_entries() {
        let toml = "[packs]\nclaude = { version = 1 }\ncodex = { version = 1 }\n";
        let resolved = resolve_project_toml(toml).unwrap();
        check_env_early(&resolved.values, &vault()).unwrap();

        let toml =
            "[packs]\nclaude = { version = 1 }\n[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n";
        let config = resolve_project_toml(toml).unwrap().values;
        let Err(e) = crate::project::resolve_env(&config, &vault()) else {
            panic!("the variable is not set");
        };
        assert_eq!(e.name, "MINE");

        let config = resolve_project_toml("presets = [\"claude-code\"]\n")
            .unwrap()
            .values;
        let Err(e) = crate::project::resolve_env(&config, &vault()) else {
            panic!("the variable is not set");
        };
        assert_eq!(e.name, "CLAUDE_CODE_OAUTH_TOKEN");
    }
}
