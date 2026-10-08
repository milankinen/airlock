use crate::project::resolve_env;
use crate::test_cfg::{ConfigDirs, host_env_vault, project_toml_error, resolve_project_toml};

#[test]
fn masked_env_from_layered_files_reaches_guest_only_as_surrogate() {
    let dirs = ConfigDirs::new();
    dirs.user_file(".airlock.toml", "[env]\nLATER = \"${HOST_LATER}\"\n")
        .project_file(
            "airlock.toml",
            r#"
            [env]
            PLAIN = "static"
            SUBST = "${HOST_TOKEN}"
            DEFAULTED = { value = "x" }
            TOKEN = { value = "${HOST_TOKEN}", mask = true }
            UNICODE = { value = "🔑-secret-token", mask = true }

            [network.rules.api]
            allow = ["api.example.com:443"]
            inject = ["TOKEN", "UNICODE"]

            [network.rules.off]
            enabled = false
            allow = ["off.example.com:443"]
            inject = ["PLAIN"]
            "#,
        )
        .project_file(".airlock/airlock.toml", "[env]\nLATER = { mask = true }\n")
        .project_file("airlock.local.toml", "[env]\nTOKEN = \"${HOST_OTHER}\"\n");
    let vault = host_env_vault(&[
        ("HOST_TOKEN", "real-value-1234"),
        ("HOST_OTHER", "other-secret-5678"),
        ("HOST_LATER", "later-secret"),
    ]);

    let env = resolve_env(&dirs.values(), &vault).unwrap();

    assert_eq!(env.guest_value("PLAIN"), Some("static"));
    assert_eq!(env.guest_value("SUBST"), Some("real-value-1234"));
    assert_eq!(env.guest_value("DEFAULTED"), Some("x"));
    assert!(env.masked("SUBST").is_none());
    let token = env.masked("TOKEN").unwrap();
    let later = env.masked("LATER").unwrap();
    let unicode = env.masked("UNICODE").unwrap();
    assert_eq!(token.real, "other-secret-5678");
    assert_eq!(later.real, "later-secret");
    assert_eq!(unicode.real, "🔑-secret-token");
    for secret in [token, later, unicode] {
        assert_eq!(secret.surrogate.len(), secret.real.len());
        assert_ne!(secret.surrogate, secret.real);
        assert!(secret.surrogate.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
    assert!(unicode.surrogate.chars().count() > unicode.real.chars().count());
    assert_eq!(
        env.guest_entries().collect::<Vec<_>>(),
        [
            ("DEFAULTED", "x"),
            ("LATER", later.surrogate.as_str()),
            ("PLAIN", "static"),
            ("SUBST", "real-value-1234"),
            ("TOKEN", token.surrogate.as_str()),
            ("UNICODE", unicode.surrogate.as_str()),
        ]
    );
    assert_eq!(env.len(), 6);
    assert_eq!(env.masked_count(), 3);
}

#[test]
fn env_entry_with_undefined_host_variable_fails_naming_key() {
    let config = resolve_project_toml("[env]\nTOKEN = { value = \"${NOPE}\", mask = true }\n")
        .unwrap()
        .values;
    let Err(err) = resolve_env(&config, &host_env_vault(&[])) else {
        panic!("undefined host variable resolves");
    };
    assert!(err.to_string().starts_with("env.TOKEN:"), "{err}");
}

#[test]
fn malformed_env_entry_is_config_error_naming_key_and_field() {
    let err = project_toml_error("[env]\nTOKEN = { mask = true }\n");
    assert!(err.contains("TOKEN"), "{err}");
    assert!(err.contains("`value`"), "{err}");

    let err = project_toml_error("[env]\nTOKEN = { value = \"x\", masked = true }\n");
    assert!(err.contains("TOKEN"), "{err}");
    assert!(err.contains("`masked`"), "{err}");

    let err = project_toml_error("[env]\nTOKEN = 1\n");
    assert!(err.contains("TOKEN"), "{err}");
}

#[test]
fn inject_of_unmasked_undefined_or_passthrough_variable_is_config_error() {
    let err = project_toml_error(
        r#"
        [env]
        A = "plain"
        TOKEN = { value = "x", mask = true }

        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["A", "B"]

        [network.rules.db]
        allow = ["db.example.com:5432"]
        passthrough = true
        inject = ["TOKEN"]
        "#,
    );
    assert_eq!(
        err,
        "invalid configuration\n\
         * `network.rules.api.inject` `A` must be defined in [env] with mask = true\n\
         * `network.rules.api.inject` `B` must be defined in [env] with mask = true\n\
         * `network.rules.db` inject cannot be combined with passthrough"
    );
}

#[test]
fn injected_value_too_short_or_not_header_safe_fails_without_leaking_it() {
    let config = resolve_project_toml(
        r#"
        [env]
        TOKEN = { value = "${T}", mask = true }

        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["TOKEN"]
        "#,
    )
    .unwrap()
    .values;
    let err = |value: &str| match resolve_env(&config, &host_env_vault(&[("T", value)])) {
        Ok(_) => panic!("{value:?} is injectable"),
        Err(e) => e.to_string(),
    };

    let short = err("abc12");
    assert!(short.starts_with("env.TOKEN:"), "{short}");
    assert!(short.contains("shorter than 8 bytes"), "{short}");
    assert!(!short.contains("abc12"), "{short}");

    let newline = err("sk-real-token-0123456789\n");
    assert!(newline.contains("HTTP header"), "{newline}");
    assert!(!newline.contains("sk-real"), "{newline}");

    resolve_env(
        &config,
        &host_env_vault(&[("T", "sk-real-token-0123456789")]),
    )
    .unwrap();
}
