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
