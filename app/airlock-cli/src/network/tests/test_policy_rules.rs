//! Tests for network policy and rules from config: allow, deny,
//! passthrough and inject decisions, config conflicts and the public-only
//! network.

use std::sync::atomic::Ordering;

use crate::config::config_values::Policy;
use crate::network::Network;
use crate::test_cfg::network::*;

/// The network of a project with `toml` as its `airlock.toml`.
fn network(toml: &str) -> Network {
    network_from_toml(toml).unwrap().network
}

/// The full error text when the network refuses `toml`. Panics if the
/// network accepts it.
fn config_error(toml: &str) -> String {
    match network_from_toml(toml) {
        Ok(_) => panic!("config accepted:\n{toml}"),
        Err(e) => format!("{e:#}"),
    }
}

/// Rules with ports, wildcards, a deny, an IPv6 literal, a passthrough
/// rule and disabled rules.
const RULES: &str = r#"
[network]
policy = "deny-by-default"

[network.rules.api]
allow = ["api.example.com:443", "*.example.org", "svc.example.net:*", "[::1]:8443"]
deny = ["secret.example.org"]

[network.rules.db]
allow = ["db.example.com:5432"]
passthrough = true

[network.rules.off]
enabled = false
allow = ["off.example.com", "api.example.com:80"]

[network.rules.off-db]
enabled = false
allow = ["offdb.example.com"]
passthrough = true
"#;

/// Test that the deny-by-default rules from config give the correct allow
/// and passthrough decision for each target. Hosts and ports that no
/// enabled rule allows must stay denied.
///   1. Build the network from rules with ports, wildcards and a deny
///   2. Resolve each host and port
///   3. Check the allowed and passthrough flags
#[test]
fn deny_by_default_rules_from_config_decide_each_target() {
    let network = network(RULES);
    for (host, port, allowed, passthrough) in [
        ("api.example.com", 443, true, false),
        ("api.example.com", 80, false, false),
        ("x.example.org", 1, true, false),
        ("a.b.example.org", 443, true, false),
        ("example.org", 443, false, false),
        ("secret.example.org", 443, false, false),
        // Case and a trailing dot must not evade the deny.
        ("SECRET.Example.org.", 443, false, false),
        ("svc.example.net", 1234, true, false),
        ("::1", 8443, true, false),
        // The localhost aliases match each other, so `[::1]` allows it.
        ("localhost", 8443, true, false),
        ("::1", 443, false, false),
        ("db.example.com", 5432, true, true),
        ("db.example.com", 5433, false, false),
        ("off.example.com", 443, false, false),
        ("offdb.example.com", 443, false, false),
        ("other.example.com", 443, false, false),
    ] {
        let target = network.resolve_target(host, port);
        assert_eq!(
            (target.allowed, target.passthrough),
            (allowed, passthrough),
            "{host}:{port}"
        );
    }
}

/// Test that the policy decides targets that no rule covers. Only
/// allow-always overrides a deny rule, and deny-always also denies targets
/// that a rule allows.
///   1. Build the network from the rules
///   2. Set each policy in turn
///   3. Check a target outside the rules and a denied target
///   4. Check that deny-always denies a target that a rule allows
#[test]
fn policy_decides_targets_outside_rules_and_allow_always_overrides_deny() {
    let network = network(RULES);
    let control = network.handle().control();
    for (policy, other, denied) in [
        (Policy::AllowByDefault, true, false),
        (Policy::AllowAlways, true, true),
        (Policy::DenyAlways, false, false),
    ] {
        control.set_policy(policy);
        assert_eq!(
            network.resolve_target("other.example.com", 443).allowed,
            other,
            "{policy:?}"
        );
        assert_eq!(
            network.resolve_target("secret.example.org", 443).allowed,
            denied,
            "{policy:?}"
        );
    }
    // The last policy in the loop is deny-always.
    assert!(!network.resolve_target("api.example.com", 443).allowed);
}

/// Test that the proxy answers HTTP 403 to a request that no allow rule
/// covers, also when the config has no allow rules.
///   1. Start a network that allows only `example.com`, then one with no
///      allow rules
///   2. Send a GET to 127.0.0.1
///   3. Check that the guest gets 403
#[test]
fn connection_outside_allow_rules_is_answered_403() {
    for allowed_hosts in [vec!["example.com".to_string()], vec![]] {
        let cfg = TestNetworkConfig {
            allowed_hosts,
            ..Default::default()
        };
        run_with_config(cfg, |proxy, _, _| async move {
            let resp = TestConnection::local(&proxy, 80)
                .await
                .roundtrip(&http_get(80, "/"))
                .await;
            assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        });
    }
}

/// Test that inject rules from config attach masked secrets only to the
/// targets of their rule. A secret on a wrong target leaks the real value.
///   1. Build the network from rules that inject secrets, with a duplicate
///      name and two rules for the same host
///   2. Resolve each host and port
///   3. Check the secret names of each target, with no duplicates
///   4. Check that the target holds the real value, not the surrogate
#[test]
fn inject_rules_from_config_attach_secrets_only_to_their_targets() {
    let built = network_from_toml(
        r#"
        [env]
        TOKEN = { value = "sk-real-token-0123456789", mask = true }
        OTHER = { value = "sk-other-0123456789", mask = true }
        PLAIN = "not-a-secret"

        [network]
        policy = "deny-by-default"

        [network.rules.api]
        allow = ["api.example.com:443", "*.example.org"]
        inject = ["TOKEN", "OTHER", "TOKEN"]

        [network.rules.also]
        allow = ["api.example.com"]
        inject = ["TOKEN"]

        [network.rules.plain]
        allow = ["plain.example.com"]
        "#,
    )
    .unwrap();
    let token = built.env.masked("TOKEN").unwrap();
    assert_ne!(token.surrogate, token.real);
    for (host, port, names) in [
        ("api.example.com", 443, vec!["TOKEN", "OTHER"]),
        ("x.example.org", 8443, vec!["TOKEN", "OTHER"]),
        ("api.example.com", 80, vec!["TOKEN"]),
        ("plain.example.com", 443, vec![]),
        ("other.example.com", 443, vec![]),
    ] {
        let target = built.network.resolve_target(host, port);
        let got: Vec<&str> = target.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(got, names, "{host}:{port}");
    }
    let target = built.network.resolve_target("api.example.com", 443);
    assert_eq!(target.secrets[0].real, token.real);
}

/// A config with an enabled and a disabled passthrough rule, plus `other`.
fn passthrough_with(other: &str) -> String {
    format!(
        r#"
        [env]
        TOKEN = {{ value = "sk-real-token-0123456789", mask = true }}

        [network.rules.pt]
        allow = ["db.example.com:5432", "*.internal.example.com"]
        passthrough = true

        [network.rules.pt-off]
        enabled = false
        allow = ["*"]
        passthrough = true

        {other}
        "#
    )
}

/// Test that the network refuses a passthrough target that overlaps a
/// middleware or inject target. A passthrough connection is never
/// intercepted, so the middleware or the secret would silently not apply.
///   1. Add each middleware or inject rule next to the passthrough rules
///   2. For an overlap, check that the error names both sides
///   3. Check that the disabled passthrough rule is not in the error
///   4. For no overlap or a disabled rule, check that the config is valid
#[test]
fn passthrough_overlapping_intercepted_target_is_rejected() {
    for (other, conflict) in [
        (
            "[network.middleware.mitm]\ntarget = [\"db.example.com:5432\"]\nscript = \"\"",
            Some(
                "rule `pt` allow=`db.example.com:5432` (passthrough) conflicts with middleware `mitm`",
            ),
        ),
        (
            "[network.middleware.mitm]\ntarget = [\"*.example.com\"]\nscript = \"\"",
            Some("middleware `mitm` target=`*.example.com`"),
        ),
        (
            "[network.middleware.mitm]\ntarget = [\"api.internal.example.com:443\"]\nscript = \"\"",
            Some("allow=`*.internal.example.com`"),
        ),
        (
            "[network.rules.inj]\nallow = [\"db.example.com\"]\ninject = [\"TOKEN\"]",
            Some("rule `inj` inject target=`db.example.com`"),
        ),
        (
            "[network.middleware.mitm]\ntarget = [\"db.example.com:443\", \"*.other.com\"]\nscript = \"\"",
            None,
        ),
        (
            "[network.middleware.mitm]\nenabled = false\ntarget = [\"db.example.com\"]\nscript = \"\"",
            None,
        ),
        (
            "[network.rules.inj]\nenabled = false\nallow = [\"db.example.com\"]\ninject = [\"TOKEN\"]",
            None,
        ),
    ] {
        let toml = passthrough_with(other);
        match conflict {
            Some(expected) => {
                let err = config_error(&toml);
                assert!(err.contains(expected), "{other}: {err}");
                assert!(!err.contains("pt-off"), "{other}: {err}");
            }
            None => {
                network_from_toml(&toml).unwrap_or_else(|e| panic!("{other}: {e:#}"));
            }
        }
    }
}

/// Test that the network refuses two reverse forwards on the same host
/// port. Only one listener can bind a host port.
///   1. Add two forwards on host port 5000 and a disabled one
///   2. Check that the error names the two enabled forwards only
///   3. Check that different host ports with the same guest port are valid
#[test]
fn reverse_forwards_sharing_host_port_are_rejected() {
    // Format is `<host>:<guest>`.
    let ports = |b: &str| {
        format!(
            "[network.ports.a]\nguest = [\"5000:4000\"]\n\
             [network.ports.b]\nguest = [\"{b}\"]\n\
             [network.ports.off]\nenabled = false\nguest = [5000]\n"
        )
    };
    let err = config_error(&ports("5000:4001"));
    assert!(err.contains("ports `a` guest=`5000:4000`"), "{err}");
    assert!(err.contains("ports `b` guest=`5000:4001`"), "{err}");
    assert!(!err.contains("ports `off`"), "{err}");
    network_from_toml(&ports("5001:4000")).unwrap();
}

/// Local destinations: loopback names and literals, private and
/// link-local addresses (cloud metadata).
const LOCAL: [&str; 9] = [
    "localhost",
    "LOCALHOST.",
    "foo.localhost",
    "127.0.0.1",
    "::1",
    "[::1]",
    "10.0.0.5",
    "192.168.1.1",
    "169.254.169.254",
];

/// Test that a public-only network denies local destinations, by name and
/// by IP literal, also under allow-always. A normal network allows them.
///   1. Build a normal and a public-only network with allow-always
///   2. Check that the normal network allows each local destination
///   3. Check that the public-only network denies each of them
///   4. Check that a public host is allowed and marked public-only
#[test]
fn public_only_network_denies_local_destinations_by_name_and_literal() {
    let toml = "[network]\npolicy = \"allow-always\"";
    let normal = network(toml);
    let public_only = network(toml).public_only();
    for host in LOCAL {
        let target = normal.resolve_target(host, 80);
        assert!(target.allowed && !target.public_only, "{host}");
        assert!(!public_only.resolve_target(host, 80).allowed, "{host}");
    }
    let public = public_only.resolve_target("example.com", 443);
    assert!(public.allowed && public.public_only);
}

/// Test that a public-only network never opens a connection to a local
/// address. A denied answer is not enough if the dial already happened.
///   1. Start a local server that counts accepted connections
///   2. Send a GET to it by three loopback names through each network
///   3. Check the 204 answers on the normal network only
///   4. Check that the server accepted no connection from the public-only
///      network
#[test]
fn public_only_network_never_dials_local_address() {
    let toml = "[network]\npolicy = \"allow-always\"";
    for (network, reachable) in [(network(toml).public_only(), false), (network(toml), true)] {
        let upstream = AcceptCounter::bind();
        let port = upstream.port();
        run_network(network, |proxy| async move {
            let accepted = upstream.start();
            // `127.1` is a short form of `127.0.0.1`.
            for host in ["127.0.0.1", "127.1", "localhost"] {
                let resp = TestConnection::connect(&proxy, host, port)
                    .await
                    .unwrap()
                    .roundtrip(&http_get(port, "/"))
                    .await;
                assert_eq!(
                    resp.starts_with("HTTP/1.1 204"),
                    reachable,
                    "{host}: {resp}"
                );
            }
            let expected = if reachable { 3 } else { 0 };
            assert_eq!(accepted.load(Ordering::SeqCst), expected);
        });
    }
}
