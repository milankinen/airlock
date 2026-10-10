# Debian

The `debian` pack uses the `debian:stable-slim` image. airlock pulls it
from the registry. It does not use a local Docker or Podman image.

```toml
[packs]
debian = { version = "1" }
```

| Arg         | Default | Description                                      |
|-------------|---------|--------------------------------------------------|
| `allow-apt` | `true`  | Allow the Debian package mirrors, for `apt install` |
