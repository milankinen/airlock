# Node.js

The `nodejs` pack installs Node.js and npm with
[nvm](https://github.com/nvm-sh/nvm). It makes Node.js trust the airlock
CA.

```toml
[packs]
nodejs = { version = "1", args = { node-version = "22" } }
```

| Arg            | Default | Description                                        |
|----------------|---------|----------------------------------------------------|
| `node-version` | `lts`   | Node.js version: `lts`, `latest`, `none` (nvm only), or an nvm version, for example `22` or `lts/jod` |
| `allow-npm`    | `true`  | Allow the npm and Yarn registries                  |
