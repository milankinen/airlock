@0x947ecba86848333b;

using Network = import "network.capnp";

# In-VM supervisor RPC. It uses the primary vsock port
# (SUPERVISOR_PORT). `boot` does not pass the `NetworkProxy`
# capability. It is the bootstrap capability of the separate network
# vsock (NETWORK_PORT). The two channels give bulk network transfers
# their own buffers, so they cannot stall the pty, stats and daemon
# traffic on this channel.
interface Supervisor {
  # Boot the VM: mount the rootfs, start networking, and start the
  # daemons. The call carries only VM and boot configuration. It has no
  # process to run. Processes (the main shell, `airlock exec`) start
  # later with `spawn`. The guest refuses `spawn` until `boot` succeeds.
  # The guest accepts `boot` one time per VM and refuses a second call.
  boot @0 (
    logs       :LogSink,
    logFilter  :Text,
    epoch      :UInt64,
    epochNanos :UInt32,
    hostPorts  :List(UInt16),
    sockets    :List(SocketForward),
    uid        :UInt32,
    gid        :UInt32,
    nestedVirt :Bool,
    harden     :Bool,
    # Mount configuration.
    imageId     :Text,
    imageLayers :List(Text),
    dirs        :List(DirMount),
    files       :List(FileMount),
    caches      :List(CacheMount),
    # Project CA cert in PEM form. Guest init appends it to the image's CA
    # bundles in an extra overlayfs lower layer when it mounts the rootfs.
    # Empty when the project has no CA. The guest then does not change the
    # CA bundles.
    caCert      :Data,
    # Sidecar processes. The boot starts them before any `spawn`. The
    # supervisor controls their lifecycle (restart loop, graceful
    # shutdown).
    daemons     :List(DaemonSpec),
    # Subdirectories of the project mount to hide from the sandbox. Guest
    # init bind-mounts an empty directory on each of them. Use it to
    # isolate parts of a monorepo from AI agents.
    masks       :List(MaskSpec),
    # Clipboard bridge grant. The default value (both flags false, null
    # `sink`) is the ungranted state. Thus a host that does not set this
    # field gives the guest nothing to call.
    clipboard   :ClipboardConfig,
    # Host browser grant. The default value (null `sink`) is the
    # ungranted state. Thus a host that does not set this field gives the
    # guest nothing to call.
    browser     :BrowserConfig,
  ) -> ();

  shutdown @1 () -> ();

  # Start a process inside the booted container, with the uid, gid and
  # harden settings from `boot`. The main shell and `airlock exec` both
  # use it. The guest refuses it until `boot` succeeds.
  spawn @2 (
    stdin :Stdin,
    pty   :PtyConfig,
    cmd   :Text,
    args  :List(Text),
    cwd   :Text,
    env   :List(Text),
  ) -> (proc :Process);

  # Sample guest CPU and memory stats for the host monitor UI. The guest
  # compares /proc/stat between two calls to get the per-core %. Thus the
  # first call returns zero per-core values.
  pollStats @3 () -> (snapshot :StatsSnapshot);

  # Tell the guest that the host denied a network request. `epoch` is
  # Unix-epoch milliseconds. The guest keeps the timestamp, so the admin
  # HTTP service at `http://admin.airlock/` can match it with Claude Code
  # tool failures that hook endpoints report.
  reportDeny @4 (epoch :UInt64) -> ();

  # Host → guest TCP port forward. The host accepted a local TCP
  # connection from a host process to a guest service. This call opens
  # TCP to 127.0.0.1:<port> inside the VM and relays bytes through the
  # sink pair. It is a raw relay with no rules and no interception. A
  # failed connect inside the guest gives a Cap'n Proto exception, and
  # the host then closes the accepted socket.
  openLocalTcp @5 (port :UInt16, client :Network.TcpSink) -> (server :Network.TcpSink);

  # Snapshot of the current state of each declared daemon. The host
  # calls it repeatedly (e.g. every 100ms) during shutdown to update the
  # per-daemon spinners. The name identifies a daemon across polls.
  pollDaemons @6 () -> (states :List(DaemonStatus));

  # Fire-and-forget: tell the supervisor to start graceful shutdown of
  # each running daemon. The host then calls `pollDaemons` until all
  # daemons are in a terminal state (`stopped` or `killed`).
  shutdownDaemons @7 () -> ();

  # Set the guest clock to the host wall-clock again. VMs have no RTC.
  # `boot @0` sets the initial clock, but long host sleeps (laptop lid
  # closed) cause the guest time to drift. The host calls this every
  # minute to keep the two clocks close.
  syncClock @8 (epoch :UInt64, epochNanos :UInt32) -> ();
}

# Host clipboard access that the host gives to the guest as a capability.
# This object is the only path from the guest to the host clipboard. Thus
# a null `ClipboardConfig.sink` denies access, whatever runs inside the
# sandbox. No guest-side flag exists that the sandbox can change.
#
# The host checks the per-direction grant and the size limit again on each
# call. The flags on `ClipboardConfig` only tell the guest which shims to
# create. They do not control access.
interface Clipboard {
  # Guest → host. The host rejects it when copy is not granted, or when
  # `data` is larger than the configured limit.
  copy  @0 (data :Data) -> ();
  # Host → guest. The host rejects it when paste is not granted.
  paste @1 () -> (data :Data);
}

struct ClipboardConfig {
  copy  @0 :Bool;
  paste @1 :Bool;
  # Null unless at least one direction is granted.
  sink  @2 :Clipboard;
  # Max bytes per guest → host copy. The host does the real check. The
  # guest gets the value so it can stop the read instead of buffering an
  # unbounded write. Without it, `cat /dev/zero > fifo` makes the guest
  # daemon (PID 1) grow until the VM dies.
  limit @3 :UInt64;
}

# Host browser access that the host gives to the guest as a capability.
# This object is the only path for the guest to tell the host to open a
# URL. Thus a null `BrowserConfig.sink` denies access, whatever runs
# inside the sandbox. No guest-side flag exists that the sandbox can
# change.
#
# The host checks each URL again against its own policy. The guest sends
# only http(s) URLs, so obvious junk does not cross the channel.
interface Browser {
  # Guest → host. The host rejects URLs that fail the host policy.
  open @0 (url :Text) -> ();
}

struct BrowserConfig {
  # Null unless the host grants browser access for this boot.
  sink @0 :Browser;
}

struct MaskSpec {
  # Mask block name from the user's config (e.g. "secret-monorepo").
  # The supervisor uses it to make the per-mask source directory path
  # /mnt/disk/mask/project/<name>.
  name             @0 :Text;
  # Project-relative paths to mask. Already validated by the host:
  # no leading `/` or `~`, no `..` segments.
  paths            @1 :List(Text);
}

struct DaemonSpec {
  name        @0 :Text;
  # argv[0] plus arguments.
  command     @1 :List(Text);
  # "KEY=VALUE" pairs. The host already merged the image env into them.
  env         @2 :List(Text);
  cwd         @3 :Text;
  # Signal sent on graceful shutdown (numeric, Linux signal number).
  signal      @4 :Int32;
  # Milliseconds to wait for the process to exit after the signal, then
  # SIGKILL. `0` means wait forever.
  timeoutMs   @5 :UInt32;
  restart     @6 :RestartPolicy;
  # Max restart attempts after the first start. `0` means no limit.
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
  # Ended cleanly (shutdown, max restarts reached, or clean exit with
  # the on-failure policy). Terminal.
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

interface LogSink {
  log @0 (level :UInt8, message :Text) -> stream;
}
