# Alpine

The `alpine` pack uses the `alpine:latest` image. airlock pulls it from the
registry. It does not use a local Docker or Podman image.

```toml
[packs]
alpine = { version = "1" }
```

| Arg         | Default | Description                                    |
|-------------|---------|------------------------------------------------|
| `allow-apk` | `true`  | Allow the Alpine package mirrors, for `apk add` |
