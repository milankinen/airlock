@0xa0085a83ad176d0f;

# Monitor feed of a running sandbox. A client gives a `MonitorSink` to
# `CliService.monitor`. The host then sends the network events, the guest
# stats and the policy changes to the sink. The client changes the policy
# with `MonitorControl`.

# Top-level network policy. The order is the order in the TUI dropdown.
enum Policy {
  allowAlways    @0;
  allowByDefault @1;
  denyByDefault  @2;
  denyAlways     @3;
}

# One HTTP header.
struct Header {
  name  @0 :Text;
  value @1 :Text;
}

# A TCP connect decision.
struct ConnectInfo {
  # Connection ID. Later events of the connection use it.
  id        @0 :UInt64;
  # Time of the connect attempt, in Unix-epoch nanoseconds.
  timestamp @1 :UInt64;
  host      @2 :Text;
  port      @3 :UInt16;
  # `true` if the policy allowed the connection.
  allowed   @4 :Bool;
}

# A connected TCP connection closed.
struct DisconnectInfo {
  # ID from `ConnectInfo.id`.
  id        @0 :UInt64;
  # Time of the close, in Unix-epoch nanoseconds.
  timestamp @1 :UInt64;
}

# Total byte counters of one connection (not deltas).
struct TrafficInfo {
  # ID from `ConnectInfo.id`.
  id   @0 :UInt64;
  # Bytes from guest to server.
  up   @1 :UInt64;
  # Bytes from server to guest.
  down @2 :UInt64;
}

# An HTTP request that the proxy saw.
struct RequestInfo {
  # Request ID. The response uses it.
  id        @0 :UInt64;
  # Time of the request, in Unix-epoch nanoseconds.
  timestamp @1 :UInt64;
  method    @2 :Text;
  path      @3 :Text;
  host      @4 :Text;
  port      @5 :UInt16;
  # `true` if the policy allowed the request.
  allowed   @6 :Bool;
  headers   @7 :List(Header);
}

# The response to a request.
struct ResponseInfo {
  # ID from `RequestInfo.id`.
  id      @0 :UInt64;
  status  @1 :UInt16;
  headers @2 :List(Header);
  # `true` if a middleware denied the request after the request event.
  denied  @3 :Bool;
}

# One network event.
struct NetworkEvent {
  union {
    connect    @0 :ConnectInfo;
    disconnect @1 :DisconnectInfo;
    traffic    @2 :TrafficInfo;
    request    @3 :RequestInfo;
    response   @4 :ResponseInfo;
  }
}

# Guest resource use.
struct StatsSnapshot {
  # CPU use of each core, 0..100.
  perCore    @0 :List(UInt8);
  totalBytes @1 :UInt64;
  usedBytes  @2 :UInt64;
  # Load average for 1, 5 and 15 minutes.
  load1      @3 :Float32;
  load5      @4 :Float32;
  load15     @5 :Float32;
}

# Kind of one diff line in an approval request.
enum DiffKind {
  same   @0;
  add    @1;
  remove @2;
}

# One line of a diff.
struct DiffLine {
  kind @0 :DiffKind;
  text @1 :Text;
}

# One labeled value in the details of an approval request.
struct ApprovalDetail {
  label @0 :Text;
  # The value. It can have more lines.
  value @1 :Text;
  # `true` if the value is code or data.
  mono  @2 :Bool;
  # Lines of a change, if the value is a diff. Not set for other values.
  diff  @3 :List(DiffLine);
}

# A request from the sandbox that the user must approve. The host cleans
# the text before it sends the request.
struct ApprovalRequest {
  title   @0 :Text;
  body    @1 :Text;
  details @2 :List(ApprovalDetail);
  # Time left for the answer, in milliseconds. After this time, the
  # client removes the dialog. A late answer has no effect.
  timeLeftMs @3 :UInt64;
}

# Receiver of the monitor feed. The client serves it. The host stops the
# feed when a call fails.
interface MonitorSink {
  # Network events in the order they occurred. The host drops events when
  # the sink is slow or the feed lags. The sink gets no notice of the
  # lost events.
  events @0 (list :List(NetworkEvent)) -> stream;

  # Guest stats, one time per second.
  stats @1 (snapshot :StatsSnapshot) -> stream;

  # The policy changed on the host.
  policy @2 (policy :Policy) -> stream;

  # Shows the request in the monitor dialog and returns the answer of the
  # user. An error tells that the dialog cannot show the request or closed
  # with no answer. The host denies the request then, and sends no more
  # requests to this sink. The host cancels the call when the time limit
  # ends.
  approve @3 (request :ApprovalRequest) -> (allow :Bool);
}

# Control of a monitor feed. The feed stops when the client releases this
# cap or disconnects.
interface MonitorControl {
  # Changes the network policy of the sandbox.
  setPolicy @0 (policy :Policy) -> ();
}
