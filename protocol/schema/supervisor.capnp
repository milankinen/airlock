@0x947ecba86848333b;

interface Supervisor {
  start @0 (
    stdin :Stdin,
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
  connect @0 (host :Text, port :UInt16, tls :Bool, client :TcpSink)
    -> (result :ConnectResult);
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
