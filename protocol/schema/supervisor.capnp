@0x947ecba86848333b;

interface Supervisor {
  ping @0 () -> (id :UInt32);
  exec @1 (stdin :ByteStream, pty :PtyConfig) -> (proc :Process);
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
