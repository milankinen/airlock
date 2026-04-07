# Add Rust, Node.js, and Python dev presets

Added development presets that allow package registry access and configure
cache mounts for build artifacts and dependencies.

- **rust**: crates.io, static.rust-lang.org; caches `~/.cargo/registry`,
  `~/.cargo/git`, `target/`
- **nodejs**: npmjs.org, yarnpkg.com; caches `~/.npm`, `~/.yarn`,
  `node_modules/`
- **python**: pypi.org, files.pythonhosted.org; caches `~/.cache/pip`,
  `.venv/`

All registries use `http:` enforcement. Cache mounts use `~` for global
caches and relative paths for project-specific dirs.
