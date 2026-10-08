# Debian

The `debian` pack uses the `debian:stable-slim` image.

```toml
[packs]
debian = { version = "1" }
```

| Arg         | Default | Description                                      |
|-------------|---------|--------------------------------------------------|
| `allow-apt` | `true`  | Allow the Debian package mirrors, for `apt install` |
