use crate::config::config::{RestartPolicy, Signal};
use crate::config::load_config::parse_config;

fn parse(toml_str: &str) -> anyhow::Result<crate::config::Config> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    parse_config(value)
}

#[test]
fn empty_daemons_ok() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"
        "#,
    )
    .unwrap();
    assert!(config.daemons.is_empty());
}

#[test]
fn defaults_applied() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [daemons.tick]
        command = ["sh", "-c", "echo tick"]
        "#,
    )
    .unwrap();
    let d = &config.daemons["tick"];
    assert!(d.enabled);
    assert_eq!(d.cwd, "/");
    assert_eq!(d.signal, Signal::Term);
    assert_eq!(d.timeout, 10);
    assert_eq!(d.restart, RestartPolicy::Always);
    assert_eq!(d.max_restarts, 10);
    assert!(d.harden);
    assert!(d.env.is_empty());
}

#[test]
fn explicit_values() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [daemons.docker]
        enabled = false
        command = ["dockerd"]
        cwd = "/var/lib/docker"
        signal = "SIGINT"
        timeout = 30
        restart = "on-failure"
        max_restarts = 0
        harden = false

        [daemons.docker.env]
        DOCKER_HOST = "unix:///var/run/docker.sock"
        "#,
    )
    .unwrap();
    let d = &config.daemons["docker"];
    assert!(!d.enabled);
    assert_eq!(d.command, vec!["dockerd"]);
    assert_eq!(d.cwd, "/var/lib/docker");
    assert_eq!(d.signal, Signal::Int);
    assert_eq!(d.timeout, 30);
    assert_eq!(d.restart, RestartPolicy::OnFailure);
    assert_eq!(d.max_restarts, 0);
    assert!(!d.harden);
    assert_eq!(d.env["DOCKER_HOST"], "unix:///var/run/docker.sock");
}

#[test]
fn invalid_signal_rejected() {
    let err = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [daemons.x]
        command = ["true"]
        signal = "SIGBOGUS"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("SIGBOGUS") || msg.contains("unknown signal"),
        "unexpected error: {msg}"
    );
}

#[test]
fn invalid_restart_rejected() {
    let err = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [daemons.x]
        command = ["true"]
        restart = "sometimes"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("sometimes") || msg.contains("restart"));
}

#[test]
fn env_templates_left_unexpanded_at_parse() {
    let config = parse(
        r#"
        [vm]
        image = "alpine:latest"

        [daemons.x]
        command = ["true"]

        [daemons.x.env]
        FOO = "${HOST_VAR}"
        "#,
    )
    .unwrap();
    assert_eq!(config.daemons["x"].env["FOO"], "${HOST_VAR}");
}

#[test]
fn signal_number_mapping() {
    assert_eq!(Signal::Hup.as_number(), 1);
    assert_eq!(Signal::Int.as_number(), 2);
    assert_eq!(Signal::Quit.as_number(), 3);
    assert_eq!(Signal::Kill.as_number(), 9);
    assert_eq!(Signal::Usr1.as_number(), 10);
    assert_eq!(Signal::Usr2.as_number(), 12);
    assert_eq!(Signal::Term.as_number(), 15);
}
