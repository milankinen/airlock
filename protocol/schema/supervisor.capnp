@0x947ecba86848333b;

interface Supervisor {
  ping @0 () -> (id :UInt32);
  openShell @1 (rows :UInt16, cols :UInt16, stdout :OutputStream)
    -> (stdin :OutputStream);
}

# Push-based byte stream. Caller pushes data via write(),
# signals completion via done() with an exit code.
interface OutputStream {
  write @0 (data :Data) -> stream;
  done @1 (exitCode :Int32) -> ();
}
