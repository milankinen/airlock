@0xaa9eb3c2a87c4a65;

# Guest-side network egress proxy.
#
# Served by the host CLI, called by the in-VM supervisor. Runs over
# its own vsock port (NETWORK_PORT) so bulk byte transfers can't
# head-of-line-block the Supervisor RPC (pty, stats, daemons).
#
# The guest receives this interface as the bootstrap capability of
# the network-side Cap'n Proto connection — it is not passed through
# `Supervisor.start` anymore.
interface NetworkProxy {
  connect @0 (target :ConnectTarget, client :TcpSink)
    -> (result :ConnectResult);
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

# Push-style byte sink. One TcpSink per connection direction:
# `client` in `connect` is pushed bytes by the remote peer (host →
# guest), the returned `server` is pushed bytes by the guest-side
# caller (guest → host).
interface TcpSink {
  send @0 (data :Data) -> stream;
  close @1 () -> ();
}
