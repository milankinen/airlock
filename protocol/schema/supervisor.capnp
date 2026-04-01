@0x947ecba86848333b;

interface Supervisor {
  start @0 (
    stdin :ByteStream,
    pty :PtyConfig,
    network :NetworkProxy,
    caCert :Data,
    caKey :Data,
    logs :LogSink
  ) -> (proc :Process);
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

interface ByteStream {
  read @0 () -> (frame :DataFrame);
}

struct DataFrame {
  union {
    eof @0 :Void;
    data @1 :Data;
    err @2 :Text;
  }
}

interface Process {
  poll @0 () -> (next :ProcessOutput);
  signal @1 (signum :UInt8) -> ();
  kill @2 () -> ();
  resize @3 (size :TermSize) -> ();
}

struct ProcessOutput {
  union {
    exit @0 :Int32;
    stdout @1 :DataFrame;
    stderr @2 :DataFrame;
  }
}

interface NetworkProxy {
  connect @0 (host :Text, port :UInt16, tls :Bool, client :TcpSink)
    -> (server :TcpSink);
}

interface TcpSink {
  send @0 (data :Data) -> stream;
  close @1 () -> ();
}

interface LogSink {
  log @0 (level :UInt8, message :Text) -> stream;
}
