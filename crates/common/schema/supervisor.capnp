@0x947ecba86848333b;

interface Supervisor {
  start @0 (
    stdin :Stdin,
    pty :PtyConfig,
    network :NetworkProxy,
    logs :LogSink,
    logFilter :Text,
    cmd :Text,
    args :List(Text),
    tlsPassthrough :List(Text),
    epoch :UInt64,
    hostPorts :List(UInt16),
    sockets :List(SocketForward)
  ) -> (proc :Process);

  shutdown @1 () -> ();

  exec @2 (
    stdin :Stdin,
    pty   :PtyConfig,
    cmd   :Text,
    args  :List(Text)
  ) -> (proc :Process);
}

# CLI server interface — exposed over the unix socket by `ez go`.
# `ez exec` connects here to attach new processes to the running container.
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
