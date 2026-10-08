# Docker

The `docker` pack installs Docker Engine, the Docker CLI, Buildx and
Compose. It starts `dockerd` as a [daemon](../../configuration/daemons.md)
in each session.

```toml
[packs]
docker = { version = "1" }
```

| Arg           | Default | Description                                         |
|---------------|---------|-----------------------------------------------------|
| `allow-pulls` | `true`  | Allow image pulls from Docker Hub and the GitHub Container Registry |

Registries also accept pushes. With `allow-pulls = true`, a process in the
sandbox can send data out through them. Set `allow-pulls = false` to block
the registries.

Docker keeps its images and build cache on the sandbox disk, so they stay
across sessions.

Containers in the sandbox use the same network rules as the sandbox.
Compose service names and published ports work as usual. To reach a
published port from the host, add a
[host → guest port forward](../../configuration/network.md#port-forwarding).
