@0xb5ce8d3c8a4a7d2f;

using Supervisor = import "supervisor.capnp";

# CLI server interface. `airlock start` serves it on a Unix socket.
# `airlock exec` connects to it to attach new processes to the running
# container. It is separate from the supervisor vsock RPC. It uses the
# process I/O types of the supervisor schema.
interface CliService {
  exec @0 (
    stdin :Supervisor.Stdin,
    pty   :Supervisor.PtyConfig,
    cmd   :Text,
    args  :List(Text),
    cwd   :Text,
    env   :List(Text),
  ) -> (proc :Supervisor.Process);
}
