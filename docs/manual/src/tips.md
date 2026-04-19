# Tips and tricks

This section collects practical patterns that come up often when working
with airlock day-to-day. None of this is required reading, but it can
save you some time.

[Pairing with mise](./tips/mise.md) shows how to use mise as a task runner
alongside airlock — installing airlock as a mise tool, building local Docker
images for sandboxes, and loading secrets per task.

[Docker inside the VM](./tips/docker.md) covers two approaches for running
Docker containers inside an airlock sandbox: forwarding the host Docker
socket (easy but comes with caveats) and running a full Docker engine inside
the VM.
