@0xaa9eb3c2a87c4a65;

# Guest-side network egress proxy.
#
# The host CLI serves it and the in-VM supervisor calls it. It uses its
# own vsock port (NETWORK_PORT), so bulk byte transfers cannot cause
# head-of-line blocking on the Supervisor RPC (pty, stats, daemons).
#
# The guest gets this interface as the bootstrap capability of the
# network Cap'n Proto connection. `Supervisor.boot` does not pass it.
interface NetworkProxy {
  connect @0 (target :ConnectTarget, client :TcpSink)
    -> (result :ConnectResult);

  # Ask the user to run one command with more network access. The request
  # gives either hosts or the name of an on-demand rule. The host shows a
  # notification and waits for the decision. A granted request returns a
  # scoped proxy. Its `connect` allows the approved hosts, or the targets
  # of the rule, in addition to the normal policy. Config deny rules still
  # apply. It refuses `socket` targets. The grant ends when the guest
  # releases the scoped proxy or when the duration expires. The host then
  # closes the live connections of the grant. A scoped proxy refuses
  # `requestGrant`.
  requestGrant @1 (req :GrantRequest) -> (result :GrantResult);

  # Get the connect targets of the grant. Only a scoped proxy serves it.
  grantLog @2 () -> (entries :List(GrantLogEntry));

  # Get all network rules of the config: normal, disabled and on-demand.
  listRules @3 () -> (rules :List(RuleInfo));

  # Ask the user to add or change a network rule. On approval, the host
  # applies the change to new connections and writes it to the project
  # config.
  upsertRule @4 (rule :RuleChange) -> (result :UpsertResult);

  # Wait until the host ends the grant. Only a scoped proxy serves it.
  # `aborted` is true if the user stopped the grant on the host.
  waitRevoked @5 () -> (aborted :Bool);
}

struct GrantRequest {
  # Shell command to run. The host writes it to the log.
  cmd   @0 :Text;
  # Requested hosts (`host` or `host:port`, `*.suffix` allowed). Only `*`
  # requests all public hosts. Empty if `rule` is set.
  hosts @1 :List(Text);
  # Name of the on-demand rule to activate. Empty if `hosts` is set.
  rule  @2 :Text;
  # More environment of the command (`KEY=VALUE`). The host writes it to
  # the log.
  env   @3 :List(Text);
  # Working directory of the command. The host writes it to the log.
  cwd   @4 :Text;
}

struct GrantResult {
  union {
    granted @0 :Grant;
    denied  @1 :Denied;
  }
}

struct Denied {
  # True if the user denied the request.
  byUser @0 :Bool;
  # Reason of the deny.
  reason @1 :Text;
}

struct Grant {
  # Proxy for the connections of the granted command.
  proxy     @0 :NetworkProxy;
  # Approved host patterns, or the allow patterns of the rule.
  hosts     @1 :List(Text);
  # Duration of the grant in seconds.
  durationSecs @2 :UInt32;
}

# One connect target of a grant.
struct GrantLogEntry {
  host    @0 :Text;
  port    @1 :UInt16;
  # True if the network denied a connection to the target.
  blocked @2 :Bool;
}

# A network rule of the config.
struct RuleInfo {
  name        @0 :Text;
  description @1 :Text;
  allow       @2 :List(Text);
  deny        @3 :List(Text);
  # `true`, `false` or `on-demand`.
  enabled     @4 :Text;
  # True if the rule relays its hosts with no interception.
  passthrough @5 :Bool;
  # True if the rule injects secrets.
  inject      @6 :Bool;
}

# A change of a network rule. Only the given values change. The other
# values of an existing rule stay.
struct RuleChange {
  name           @0 :Text;
  description    @1 :Text;
  allow          @2 :List(Text);
  deny           @3 :List(Text);
  # `true`, `false` or `on-demand`. Empty: keep the value of an existing
  # rule. A new rule is `on-demand`.
  enabled        @4 :Text;
  setDescription @5 :Bool;
  setAllow       @6 :Bool;
  setDeny        @7 :Bool;
}

struct UpsertResult {
  union {
    approved @0 :Void;
    denied   @1 :Denied;
    # The request is not valid, or the host could not apply it.
    error    @2 :Text;
  }
}

struct ConnectTarget {
  union {
    tcp @0 :TcpTarget;
    socket @1 :Text;
  }
}

struct TcpTarget {
  host @0 :Text;
  port @1 :UInt16;
}

struct ConnectResult {
  union {
    server @0 :TcpSink;
    denied @1 :Text;
  }
}

# Push-style byte sink. Each connection direction has one TcpSink.
# The `client` sink of `connect` gets the bytes from the remote peer
# (host → guest). The returned `server` sink gets the bytes from the
# guest-side caller (guest → host).
interface TcpSink {
  send @0 (data :Data) -> stream;
  close @1 () -> ();
}
