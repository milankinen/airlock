# GitHub Copilot CLI

The `copilot` pack installs the
[GitHub Copilot CLI](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli)
into the sandbox. The ACP support is built in (`copilot --acp`). See [editor integration](../../tips/editor-acp.md).

```toml
[packs]
copilot = { version = "1" }
```

```bash
airlock start --monitor -- copilot
```

The pack has no args.

## Authentication

### GitHub token

Create a [fine-grained personal access token][new-pat] with the
**Account > Copilot Requests** permission. Store it in the airlock
[secret vault](../../secrets.md):

[new-pat]: https://github.com/settings/personal-access-tokens/new

```bash
airlock secrets add COPILOT_GITHUB_TOKEN
```

airlock reads the token from the host environment first and then from the
vault. If neither has the token, `airlock start` stops with an error.

The sandbox sees only a [masked](../../configuration/env.md#masking)
surrogate, so you do not need `/login`. airlock puts the real token into
the requests. For the supported token types, see the
[Copilot CLI authentication docs][copilot-auth].

[copilot-auth]: https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli#authenticating-with-environment-variables
