# Confirmation prompts as abort-first lists

All CLI questions now use one prompt style and one word for "stop".

## Change

- The yes/no prompt (inline `« yes · no »` bar, ←→ keys) is removed. Its
  four users now use the radio list prompt through a new `choose::confirm`
  helper. The options are `Abort` (default, first) and the action option.
  No option has its own color. Only the focused option is highlighted.
  An accidental Enter thus never removes data.
  - `airlock rm`: `Remove sandbox?` / `Yes, remove` in all cases. The
    notes about the local config and the symlinked `.airlock` are gone
    from the question. The report after the removal still tells what was
    removed.
  - `airlock sandboxes rm`: `Remove sandbox <id>?` with the gray note
    `Project: <path>`, and `Yes, remove`. The message after an abort keeps the id, because
    the command can remove many sandboxes in one run.
  - `airlock templates rm`: `Remove template <name>?` / `Yes, remove`.
  - `airlock secrets add` with a plaintext vault:
    `Proceed with the plaintext vault?` / `Yes, proceed`.
- "Cancel" is now "abort" in all CLI prompts: option labels, key help
  (`esc abort`), wizard start bar (`start · abort`), messages and internal
  names. The monitor key action `cancel` does not change, because it is a
  user settings key.
- All list option labels start with a capital letter (`Abort`,
  `Re-create sandbox`). Some prompts had lowercase labels.
- The wizard start bar (`start · abort`, `create · abort`) stays
  lowercase, like the pack rows. Its current option is cyan, like a
  focused label. Before, it was green, and `abort` was red. No prompt
  uses color for the kind of an option now, so the option tone is
  removed.
- The gray notes of the list prompt have no indent. They align with the
  title, like the details hint.
- `airlock secrets add`: an abort (plaintext vault question or Esc on the
  value prompt) prints `Aborted.` and exits with code 0. Before, it
  printed `Error: cancelled` and exited with code 1. A typed error
  carries the abort to the command.
- The vault passphrase prompt still fails with an error on Esc. It also
  runs inside `airlock start`, where the vault must open.
