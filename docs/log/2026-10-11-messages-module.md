# CLI texts in one messages module

All texts that the CLI shows to the user are now in one module. It is
easier to review and keep the wording the same when all texts are in one
place.

## Change

- New module `app/airlock-cli/src/messages.rs`. It has one inline
  `pub mod` for each area of the CLI (`cli`, `oci`, `start`, `vault` and
  others), about 700 items in total.
  - A fixed text is a `pub const`. A text with values is a `pub fn` that
    returns a `String` and takes `impl Display` arguments.
  - The texts have no marks, indents or colors. The call site adds them,
    for example `cli::log!("  {} {}", mark, messages::oci::image_cached(r))`.
  - Errors use the items directly: `bail!(messages::x::foo(a))`,
    `.context(messages::x::FOO)`, `#[error("{}", crate::messages::x::FOO)]`.
- Included: status, verbose and error messages, prompt titles, notes,
  options and key help, progress texts, stdout output and error texts
  (`bail!`, `anyhow!`, `ensure!`, `.context`, `#[error]`).
- Not included: `tracing` logs, panic texts, test code, clap help, texts
  that go to the sandbox (guest errors, HTTP bodies, Lua errors) and
  machine output.
- `info --json` keeps its status values as serde enums in `cmd_info.rs`.
  Only the human output maps them to message texts. Thus a text change
  never changes the JSON.
- Tests compare against literal texts, not against message items. Thus a
  text change shows in the tests.
- The output text did not change, with one exception: `airlock rm` now
  prints only `Sandbox removed` (or `No sandbox to remove`). The local
  config, the kept user files and the `.airlock` link target are not
  named any more. The code that found these names is removed.
- CLAUDE.md has a new section "CLI output texts" with these rules.
