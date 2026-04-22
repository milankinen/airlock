@0xb5ce8d3c8a4a7d2f;

using Supervisor = import "supervisor.capnp";

# CLI server interface — exposed over the unix socket by `airlock start`.
# `airlock exec` connects here to attach new processes to the running
# container. Unrelated to the supervisor's vsock RPC; shares process
# I/O primitives via the import.
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
