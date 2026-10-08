use smart_config::ByteSize;

use crate::config::config_values::{PullPolicy, Resolution, RestartPolicy, Signal};
use crate::test_cfg::{project_toml_error, resolve_project_toml};

#[test]
fn config_without_sections_grants_no_clipboard_kvm_services_or_daemons() {
    for toml in ["", "[clipboard]\n"] {
        let config = resolve_project_toml(toml).unwrap().values;
        assert!(!config.clipboard.copy, "{toml}");
        assert!(!config.clipboard.paste, "{toml}");
        assert_eq!(config.clipboard.copy_limit, ByteSize(1024 * 1024));
        assert!(!config.vm.kvm);
        assert!(config.network.services.is_empty());
        assert!(config.daemons.is_empty());
    }
}

#[test]
fn project_file_sections_resolve_to_their_values() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let config = resolve_project_toml(&format!(
        r#"
        [vm]
        kvm = true

        [vm.image]
        name = "alpine:3.20@{digest}"
        resolution = "podman"
        pull_policy = "if-changed"

        [clipboard]
        copy = true
        copy_limit = "2 MB"

        [daemons.tick]
        command = ["sh", "-c", "echo tick"]

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
        FROM_HOST = "${{HOST_VAR}}"

        [network.rules.api]
        allow = ["api.example.com", "api.example.com:443", "api.example.com:*", "*:80", "*"]
        deny = ["internal.example.com:*", "[::1]:8080"]

        [network.middleware.mw]
        target = ["api.example.com:*"]
        script = ""

        [network.ports.app]
        host = [8080, "9000:8081"]

        [network.sockets.docker]
        host = "~/.docker/run/docker.sock:/var/run/docker.sock"

        [network.sockets.same]
        host = "/run/agent.sock"
        "#
    ))
    .unwrap()
    .values;

    assert!(config.vm.kvm);
    let image = &config.vm.image;
    assert!(matches!(image.resolution, Resolution::Podman));
    assert_eq!(image.pull_policy, PullPolicy::IfChanged);
    assert_eq!(image.pinned_digest(), Some(digest.as_str()));

    assert!(config.clipboard.copy);
    assert!(!config.clipboard.paste);
    assert_eq!(config.clipboard.copy_limit, ByteSize(2 * 1024 * 1024));

    let tick = &config.daemons["tick"];
    assert!(tick.enabled);
    assert_eq!(tick.cwd, "/");
    assert_eq!(tick.signal, Signal::Term);
    assert_eq!(tick.timeout, 10);
    assert_eq!(tick.restart, RestartPolicy::Always);
    assert_eq!(tick.max_restarts, 10);
    assert!(tick.harden);
    assert!(tick.env.is_empty());
    let docker = &config.daemons["docker"];
    assert!(!docker.enabled);
    assert_eq!(docker.command, ["dockerd"]);
    assert_eq!(docker.cwd, "/var/lib/docker");
    assert_eq!(docker.signal, Signal::Int);
    assert_eq!(docker.timeout, 30);
    assert_eq!(docker.restart, RestartPolicy::OnFailure);
    assert_eq!(docker.max_restarts, 0);
    assert!(!docker.harden);
    assert_eq!(docker.env["DOCKER_HOST"], "unix:///var/run/docker.sock");
    assert_eq!(docker.env["FROM_HOST"], "${HOST_VAR}");

    assert_eq!(config.network.rules["api"].allow.len(), 5);
    let ports: Vec<_> = config.network.ports["app"]
        .host
        .iter()
        .map(|p| (p.host, p.guest))
        .collect();
    assert_eq!(ports, [(8080, 8080), (9000, 8081)]);
    let socket = &config.network.sockets["docker"].host;
    assert_eq!(socket.source, "~/.docker/run/docker.sock");
    assert_eq!(socket.target, "/var/run/docker.sock");
    let same = &config.network.sockets["same"].host;
    assert_eq!(
        (same.source.as_str(), same.target.as_str()),
        ("/run/agent.sock", "/run/agent.sock")
    );
}

#[test]
fn image_reference_forms_resolve_pull_policy_and_resolution() {
    let image = |toml: &str| resolve_project_toml(toml).unwrap().values.vm.image;

    let plain = image("[vm]\nimage = \"alpine:latest\"\n");
    assert_eq!(plain.name, "alpine:latest");
    assert_eq!(plain.pull_policy, PullPolicy::IfNotPresent);
    assert!(matches!(plain.resolution, Resolution::Auto));
    assert_eq!(plain.pinned_digest(), None);

    let kebab = image("[vm.image]\nname = \"alpine:latest\"\npull-policy = \"if-changed\"\n");
    assert_eq!(kebab.pull_policy, PullPolicy::IfChanged);

    let registry = image("[vm.image]\nname = \"alpine:latest\"\nresolution = \"registry\"\n");
    assert_eq!(registry.pull_policy, PullPolicy::IfNotPresent);
    assert!(matches!(registry.resolution, Resolution::Registry));
}

#[test]
fn invalid_section_values_are_config_errors_naming_their_path() {
    for (toml, path) in [
        (
            "[clipboard]\ncopy_limit = \"banana\"\n",
            "clipboard.copy_limit",
        ),
        ("[clipboard]\ncopy = \"yes\"\n", "clipboard.copy"),
        (
            "[daemons.x]\ncommand = [\"true\"]\nsignal = \"SIGBOGUS\"\n",
            "unknown signal 'SIGBOGUS'",
        ),
        (
            "[daemons.x]\ncommand = [\"true\"]\nrestart = \"sometimes\"\n",
            "restart",
        ),
        (
            "[vm.image]\nname = \"alpine:latest\"\npull-policy = \"always\"\n",
            "vm.image",
        ),
        (
            "[network.services]\nanthropic = \"yes\"\n",
            "network.services",
        ),
    ] {
        let err = project_toml_error(toml);
        assert!(err.contains("invalid configuration"), "{toml}: {err}");
        assert!(err.contains(path), "{toml}: {err}");
    }
}

#[test]
fn malformed_network_targets_and_unknown_services_are_reported_together() {
    let err = project_toml_error(
        r#"
        [network.services]
        anthropic = true
        gemini = true

        [network.rules.api]
        allow = ["*:8O80", "b.example.com:y"]
        deny = ["internal.example.com:https"]

        [network.rules.off]
        enabled = false
        allow = ["*:8O80"]

        [network.middleware.mw]
        target = ["api.example.com:443 "]
        script = ""
        "#,
    );
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines[0], "invalid configuration");
    assert_eq!(
        lines[1],
        "* `network.services.gemini` unknown service (known: anthropic, openai)"
    );
    assert!(
        lines[2].starts_with("* `network.rules.api.allow` "),
        "{err}"
    );
    assert!(lines[2].contains("`*:8O80`"), "{err}");
    assert!(lines[2].contains("port `8O80`"), "{err}");
    assert!(
        lines[3].starts_with("* `network.rules.api.allow` "),
        "{err}"
    );
    assert!(lines[3].contains("`b.example.com:y`"), "{err}");
    assert!(lines[4].starts_with("* `network.rules.api.deny` "), "{err}");
    assert!(lines[4].contains("`internal.example.com:https`"), "{err}");
    assert!(
        lines[5].starts_with("* `network.middleware.mw.target` "),
        "{err}"
    );
    assert!(lines[5].contains("`api.example.com:443 `"), "{err}");
    assert_eq!(lines.len(), 6, "{err}");
}

#[test]
fn passthrough_rule_on_enabled_service_host_is_error() {
    let network = |on: bool| {
        resolve_project_toml(&format!(
            "[network.services]\nanthropic = {on}\n\
             [network.rules.raw]\nallow = [\"*.anthropic.com\"]\npassthrough = true\n"
        ))
        .unwrap()
        .values
        .network
    };
    let err = format!(
        "{:#}",
        crate::network::check_passthrough(&network(true)).unwrap_err()
    );
    assert!(
        err.contains(
            "rule `raw` allow=`*.anthropic.com` (passthrough) conflicts with service \
             `anthropic` host `api.anthropic.com:443`"
        ),
        "{err}"
    );
    crate::network::check_passthrough(&network(false)).unwrap();
}
