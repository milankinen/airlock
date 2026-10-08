# mise

The `mise` pack installs the [mise](https://mise.jdx.dev/) tool and
runtime version manager. Login shells get the mise shims on the `PATH`.

```toml
[packs]
mise = { version = "1" }
```

The pack has no args. It allows no hosts. Add network rules for the
tools that mise installs, or run the installs with an
[open network](../../tips/init-with-open-network.md).
