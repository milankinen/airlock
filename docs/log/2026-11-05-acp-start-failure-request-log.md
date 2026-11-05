# Log the request on ACP agent start failure

`airlock acp` logged only "agent start failed: no sandbox for <dir>" when a
client session request pointed to a dir with no sandbox (for example old
git worktrees). The log did not show which client call caused it.

The warning now also has `method` and `params` of the first request of the
session. The params are cut to 500 characters, because a request can carry
prompt text or other large payloads.
