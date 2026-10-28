@0xb5ce8d3c8a4a7d2f;

using Network = import "network.capnp";
using Supervisor = import "supervisor.capnp";

# CLI server interface. `airlock start` serves it on a Unix socket for
# clients that attach to the running sandbox: `airlock exec` (new
# processes in the container), `airlock network forward` (ad-hoc port
# forwards) and `airlock acp` (host MCP servers for an editor agent). It
# is separate from the supervisor vsock RPC. It uses the process I/O and
# port forward types of the supervisor schema.
interface CliService {
  exec @0 (
    stdin :Supervisor.Stdin,
    pty   :Supervisor.PtyConfig,
    cmd   :Text,
    args  :List(Text),
    cwd   :Text,
    env   :List(Text),
  ) -> (proc :Supervisor.Process);

  # Opens one connection to a guest loopback port. Serves
  # `--mode=guest`: the CLI calls it once for each accepted host
  # connection. The server relays the call to
  # `Supervisor.connectToGuestPort`.
  connectToGuestPort @1 (port :UInt16, client :Network.TcpSink) -> (server :Network.TcpSink);

  # Binds a guest loopback port and relays each accepted connection to
  # the host. Serves `--mode=host`. The server relays the call to
  # `Supervisor.bindHostPort`. The guest listener closes when the client
  # releases `handle` or disconnects.
  bindHostPort @2 (port :UInt16, connector :Supervisor.LocalConnector) -> (handle :Supervisor.ListenHandle);

  # Serves one more host MCP server to the guest. The server checks
  # `session` and `name` and passes the call to `Supervisor.addMcpServer`.
  # The route closes when the client releases `grant` or disconnects.
  addMcpServer @3 (session :Text, name :Text, host :Supervisor.McpHost) -> (grant :Supervisor.McpGrant);
}
