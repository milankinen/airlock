@0x947ecba86848333b;

interface Supervisor {
  start @0 (
    stdin      :Stdin,
    pty        :PtyConfig,
    network    :NetworkProxy,
    logs       :LogSink,
    logFilter  :Text,
    epoch      :UInt64,
    epochNanos :UInt32,
    hostPorts  :List(UInt16),
    sockets    :List(SocketForward),
    # Process configuration (replaces config.json)
    cmd        :Text,
    args       :List(Text),
    env        :List(Text),
    cwd        :Text,
    uid        :UInt32,
    gid        :UInt32,
    nestedVirt :Bool,
    harden     :Bool,
    # Mount configuration (replaces mounts.json)
    imageId     :Text,
    imageLayers :List(Text),
    dirs        :List(DirMount),
    files       :List(FileMount),
    caches      :List(CacheMount),
    # Project CA cert in PEM form. Appended to the image's CA bundles by
    # guest init after the overlayfs rootfs is mounted. Empty when the
    # project has no CA (vault disabled / TLS interception off).
    caCert      :Data,
    # Sidecar processes to start in parallel with the main shell. The
    # supervisor owns their lifecycle (restart loop, graceful shutdown).
    daemons     :List(DaemonSpec),
  ) -> (proc :Process);

  shutdown @1 () -> ();

  exec @2 (
    stdin :Stdin,
    pty   :PtyConfig,
    cmd   :Text,
    args  :List(Text),
    cwd   :Text,
    env   :List(Text),
  ) -> (proc :Process);

  # Sample guest CPU and memory stats for the host monitor UI. The
  # implementation diffs /proc/stat across consecutive calls to compute
  # per-core %; the first call returns zeroed per-core values.
  pollStats @3 () -> (snapshot :StatsSnapshot);

  # Host-to-guest notification that a network request was just denied.
  # `epoch` is Unix-epoch milliseconds. The guest caches the timestamp
  # so the admin HTTP service at `http://admin.airlock/` can correlate
  # it with Claude Code tool failures reported via hook endpoints.
  reportDeny @4 (epoch :UInt64) -> ();

  # Host → guest TCP port forward. The host has accepted a local TCP
  # connection from some host process destined for a guest service;
  # this opens TCP to 127.0.0.1:<port> inside the VM and bridges bytes
  # via the sink pair. Raw relay — no rules, no interception. Failures
  # to connect inside the guest surface as Cap'n Proto exceptions so
  # the host closes the accepted socket.
  openLocalTcp @5 (port :UInt16, client :TcpSink) -> (server :TcpSink);

  # Snapshot of every declared daemon's current state. Called repeatedly
  # (e.g. every 100ms) by the host during shutdown UI to drive per-daemon
  # spinners. Daemons are identified by name across polls.
  pollDaemons @6 () -> (states :List(DaemonStatus));

  # Fire-and-forget: ask the supervisor to start graceful shutdown for
  # every still-running daemon. Host follows up with `pollDaemons` until
  # all daemons reach a terminal state (`stopped` or `killed`).
  shutdownDaemons @7 () -> ();
}

struct DaemonSpec {
  name        @0 :Text;
  # argv[0] plus arguments.
  command     @1 :List(Text);
  # "KEY=VALUE" pairs. Image env is already layered in by the host.
  env         @2 :List(Text);
  cwd         @3 :Text;
  # Signal sent on graceful shutdown (numeric, Linux signal number).
  signal      @4 :Int32;
  # Milliseconds to wait for the process to exit after the signal, then
  # SIGKILL. `0` means wait forever.
  timeoutMs   @5 :UInt32;
  restart     @6 :RestartPolicy;
  # Max restart attempts after the initial launch. `0` = no cap.
  maxRestarts @7 :UInt32;
  # Per-daemon hardening override. Independent of the main-shell toggle.
  harden      @8 :Bool;
}

enum RestartPolicy {
  always    @0;
  onFailure @1;
}

enum DaemonState {
  # Currently alive, or between restarts inside the restart loop.
  running @0;
  # Terminated cleanly (shutdown, max-restarts reached, or on-failure
  # clean exit). Terminal.
  stopped @1;
  # SIGKILL'd after the graceful-shutdown timeout elapsed. Terminal.
  killed  @2;
}

struct DaemonStatus {
  name  @0 :Text;
  state @1 :DaemonState;
}

struct StatsSnapshot {
  cpu         @0 :CpuStats;
  memory      @1 :MemoryStats;
  loadAverage @2 :LoadAverage;
}

struct CpuStats {
  # Per-core utilization 0..100 at snapshot time.
  perCore @0 :List(UInt8);
}

struct MemoryStats {
  totalBytes @0 :UInt64;
  usedBytes  @1 :UInt64;
}

struct LoadAverage {
  one     @0 :Float32;
  five    @1 :Float32;
  fifteen @2 :Float32;
}

# CLI server interface — exposed over the unix socket by `airlock go`.
# `airlock exec` connects here to attach new processes to the running container.
interface CliService @0xb5ce8d3c8a4a7d2f {
  exec @0 (
    stdin :Stdin,
    pty   :PtyConfig,
    cmd   :Text,
    args  :List(Text),
    cwd   :Text,
    env   :List(Text)
  ) -> (proc :Process);
}

struct SocketForward {
  host @0 :Text;
  guest @1 :Text;
}

struct DirMount {
  tag      @0 :Text;
  target   @1 :Text;
  readOnly @2 :Bool;
}

struct FileMount {
  target   @0 :Text;
  readOnly @1 :Bool;
  key      @2 :Text;
}

struct CacheMount {
  name    @0 :Text;
  enabled @1 :Bool;
  paths   @2 :List(Text);
}

struct PtyConfig {
  union {
    none @0 :Void;
    size @1 :TermSize;
  }
}

struct TermSize {
  rows @0 :UInt16;
  cols @1 :UInt16;
}

interface Stdin {
  read @0 () -> (input :ProcessInput);
}

interface Process {
  poll @0 () -> (next :ProcessOutput);
  signal @1 (signum :Int32) -> ();
  kill @2 () -> ();
}

struct ProcessInput {
  union {
    stdin @0 :DataFrame;
    resize @1 :TermSize;
  }
}

struct ProcessOutput {
  union {
    exit @0 :Int32;
    stdout @1 :DataFrame;
    stderr @2 :DataFrame;
  }
}

struct DataFrame {
  union {
    eof @0 :Void;
    data @1 :Data;
  }
}

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

interface TcpSink {
  send @0 (data :Data) -> stream;
  close @1 () -> ();
}

interface LogSink {
  log @0 (level :UInt8, message :Text) -> stream;
}
