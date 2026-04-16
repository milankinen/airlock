# Plan: Add `network.ports` config for localhost port forwarding

## Context

Localhost port forwarding has a bug: when the guest proxy intercepts a connection to `127.0.0.1:18080`, it sends the literal IP `"127.0.0.1"` to the host proxy via RPC. The host proxy's `resolve_target()` checks this against allow rules using `host_matches()`, which does exact string comparison — so `host_matches("127.0.0.1", "localhost")` returns false, and the connection is denied.

The root cause is a design issue: port forwarding is overloaded onto network allow rules (`[network.rules.localhost] allow = ["localhost:18080"]`). This conflates two different concerns: "which external connections are allowed" vs "which host ports should be forwarded into the VM". The fix is a dedicated `network.ports` config section that:
1. Declares which host ports the guest can reach (with optional guest->host port mapping)
2. Automatically allows those ports in the host proxy (bypassing rule matching entirely)
3. Passes port info to the guest for iptables setup

## High-level design

New config section:
```toml
[network.ports.dev-server]
host = [8080, "9000:8081"]  # guest_port:host_port mapping
```

- Simple integer `8080` means same port on both sides (host 8080 -> guest 8080)
- String `"9000:8081"` means host port 9000 maps to guest port 8081 (source:target)
- Port forwarding connections bypass the normal allow/deny rule engine entirely
- The existing `localhost_ports_from_config()` approach (scanning rules for localhost targets) is replaced by reading from `network.ports`

## Detailed changes

### 1. Config: add `PortForward` struct and `ports` field

**File:** `crates/airlock/src/config.rs` (within `mod config`)

Add `PortForward` struct with `enabled` flag and `host: Vec<PortMapping>`.
Add `PortMapping` newtype wrapping `(u16, u16)` (guest_port, host_port) with custom deserialization accepting both integer and `"guest:host"` string forms.
Add `ports: BTreeMap<String, PortForward>` to `Network` struct.

### 2. Replace `localhost_ports_from_config()` with `port_forwards_from_config()`

**File:** `crates/airlock/src/network/rules.rs`

New function returns `Vec<(u16, u16)>` from `network.ports` config.

### 3. Remove dead kernel cmdline parameters

**File:** `crates/airlock/src/vm.rs`

Remove `airlock.epoch`, `airlock.shares`, `airlock.host_ports` from kernel cmdline.
These are all passed via RPC already; `airlockd` never reads `/proc/cmdline`.

### 4. Update RPC to pass guest ports from new config

**File:** `crates/airlock/src/rpc/supervisor.rs`

Keep existing `hostPorts :List(UInt16)` Cap'n Proto field, change data source.

### 5. Auto-allow port-forwarded connections in the host proxy

**File:** `crates/airlock/src/network.rs`

Add `port_forwards: HashMap<u16, u16>` to `Network` struct.
Update `resolve_target()` to check port forwards before allow/deny rules.

### 6. Update network test + manual documentation

**Files:** `tests/vm/network.bats`, `docs/manual/`

## Files to modify

1. `crates/airlock/src/config.rs`
2. `crates/airlock/src/config/de.rs`
3. `crates/airlock/src/network/rules.rs`
4. `crates/airlock/src/network.rs`
5. `crates/airlock/src/vm.rs`
6. `crates/airlock/src/rpc/supervisor.rs`
7. `tests/vm/network.bats`
8. `docs/manual/`
