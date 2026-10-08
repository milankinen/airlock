# Alpine

The `alpine` pack uses the `alpine:latest` image.

```toml
[packs]
alpine = { version = "1" }
```

| Arg         | Default | Description                                    |
|-------------|---------|------------------------------------------------|
| `allow-apk` | `true`  | Allow the Alpine package mirrors, for `apk add` |
