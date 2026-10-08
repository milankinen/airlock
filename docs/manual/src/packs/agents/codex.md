# OpenAI Codex

The `codex` pack installs the
[OpenAI Codex CLI](https://github.com/openai/codex) into the sandbox.

```toml
[packs]
codex = { version = "1" }
```

```bash
airlock start --monitor -- codex --yolo
```

| Arg   | Default | Description                                        |
|-------|---------|----------------------------------------------------|
| `acp` | `false` | Install the ACP adapter as the `codex-acp` command. See [editor integration](../../tips/editor-acp.md). |

## Authentication

### Login in the application

Run `codex login` in the sandbox. The sign-in page opens in the browser of
the host. `codex login --device-auth` also works.

airlock keeps the real tokens on the host, encrypted with a key from the
[secret vault](../../secrets.md). Codex gets only surrogate tokens.

The sign-in is shared:

- A sign-in in one sandbox applies to all sandboxes with the `codex`
  pack.
- A logout in one sandbox signs out all sandboxes.

### API key

Set `OPENAI_API_KEY` on the host, or store it in the
[secret vault](../../secrets.md):

```bash
airlock secrets add OPENAI_API_KEY
```

Codex then uses it automatically. The real value stays on the host. Codex
gets only a surrogate.
