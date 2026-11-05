# Sandbox notice log and config change notices

`airlock exec` and `airlock acp` start a stopped sandbox with the stored
config of the last `airlock start`. They do not apply config changes, and
the user did not know when the project config was different. Now the
start adds a notice to the notice log of the sandbox, and each client
shows the notices of the log.

## Notice log

* The sandbox process keeps one notice log for its run (`NoticeLog`).
  Each notice is `{seqno, text}`, text only. The numbers start at 1 and
  grow by one.
* The log only grows. Nothing removes or changes a notice. Thus each
  client reads the full log at each read and finds new notices by their
  number.
* Sources: the config check of the exec and acp start, and the browser
  bridge. The browser bridge had its own queue, which each read took
  (`take_notices`). That queue is gone. Browser texts start with
  `browser: `, because a notice has no source field.
* `push` makes each text one line of plain text, for all sources: it
  removes the ANSI sequences, changes whitespace controls (`\n`, `\r`,
  `\t`) to spaces, removes the other control characters (also a lone
  ESC) and the text direction marks, and cuts the text after 1000
  characters with the mark ` [...]`. Code in the sandbox can cause
  notices (browser requests), and the clients print them on the
  terminal.
* `push` skips a text that the log already has. Thus repeated refused
  browser requests cannot fill the log.
* Limit: 32 notices. The next notice gives one last notice
  (`NOTICE_LIMIT`: the sandbox does not show more notices, the sandbox log
  has them), and after it the log takes nothing. The rejected notices go
  to the sandbox log. A changed "N more" counter at the end would change
  a notice that a client already showed. The limit notice is a plain new
  notice, so the log stays append-only and the numbers stay stable.

## Config check

1. In exec and acp mode, the sandbox process resolves the stored last-run
   config as before (`prepare_last_run`).
2. It then loads the current project config with no questions
   (`config_change::current_differs`): plain load (no wizard), resolve
   with `ApprovalMode::Refuse` and no overrides, resolved host paths.
3. It compares with the same rule as the `airlock start` check
   (`unchanged`: equal final values and no moved host paths). The rule
   is one helper for both, so they cannot drift apart.
4. A difference adds `CONFIG_CHANGED` to the notice log. A load or
   resolve error, a missing project config, or a pack without approval
   also counts as a change. The reason goes to the log. The check never
   prompts, writes nothing and never blocks the start.
5. `resolve_last_run` returns the stored values and paths, so `run.json`
   is read only once.

## RPC

* `notices @7` returns `List(Notice)` with
  `struct Notice { seqno :UInt64, text :Text }`, all notices since the
  boot. It took the browser notices before and returned `List(Text)`.
* A client does not check the version of the sandbox process at the
  connect. A client and a sandbox process of different versions cannot
  read the other's list. The read fails, the client shows no notices,
  and the error goes to the log. This was accepted: it applies only
  until the old sandbox stops.

## Quiet exec and acp start

The sandbox process of exec and acp always runs quiet
(`LaunchOpts::for_client`). It shows no status and no progress, only the
vault question and the errors. Thus the start does not show:

* "Booting sandbox...", "Preparing sandbox" and the config report lines
* the disk, image, mount and daemon status lines
* the spinners and progress bars, for example the daemon stop

On the client side, `ensure_with` (exec and acp) still shows "waiting
for the sandbox" on stderr. It shows only while another process starts
or stops the sandbox, so a normal start shows nothing extra.

## Warnings

A warning uses `cli::warn!(text, line...)`. If the process has a notice
receiver (`cli::set_notice_sink`), the text without the mark goes to it,
and the log removes the colors. Else the line prints as before. The sandbox process sends its
warnings to the notice log:

* exec and acp: from the start of `serve()`, before the start work.
* all modes: after the detach. Then the process has no terminal, and
  before, later warnings were lost. `airlock start` shows its warnings
  on the terminal until the detach, as before, with no copy in the log.

The receiver is global (a mutex), because a warning can come from any
thread. Thus `NoticeLog` is `Arc<Mutex<..>>`.

Warnings that use `cli::warn!`:

* unknown pack directories of the settings (`packs::init`, also in
  `prepare_last_run`)
* services that are not available (`services::warn_once`)
* a skipped mount with `missing = "warn"` (`vm/mount.rs`)
* a file mount that is a copy, not a link (`vm/mod.rs`)
* no clipboard program on the host (`rpc/clipboard.rs`)
* a replaced host socket (`network/host_socket.rs`, at run time)
* a kept larger disk (`vm/disk.rs`, `airlock start` only)
* an old `.airlock` dir that the import cannot remove
  (`sandbox/migrate.rs`, `airlock start` only)

Not changed: the data dir move warning (`cache.rs`) runs in the client
before a sandbox process exists, and the auto-approve line of packs is a
status line of `airlock start --auto-approve-packs`.

## Rendering

* exec: after the connect and before the command, all notices of the log
  as yellow `!` warnings on stderr. This also covers a connect to a
  running sandbox that started with a changed config.
* start: all notices after the connect and before the main command, then
  after the main command only the notices with a higher number.
* acp: see the acp code (`acp/tests/test_sandbox_notices.rs`).

## Limits

* No test covers the full process path (real sandbox process start). The
  compare has tests in `sandbox/start/tests/test_config_change.rs`, the
  log and the RPC in `sandbox/process/tests/test_notice_log.rs`.
