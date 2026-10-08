# Python

The `python` pack installs Python and the
[uv](https://docs.astral.sh/uv/) package manager. It makes Python, pip
and uv trust the airlock CA.

```toml
[packs]
python = { version = "1", args = { python-version = "3.12" } }
```

| Arg              | Default  | Description                                    |
|------------------|----------|------------------------------------------------|
| `python-version` | `latest` | Python version: `latest`, `3.13`, `3.12`, `none` (uv only), or a uv Python request, for example `3.12.4` or `pypy@3.11` |
| `allow-pypi`     | `true`   | Allow PyPI                                     |
