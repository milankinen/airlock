# Shell completions

## Goal

Complete the commands and options of the CLI, and also values that exist
only at run time: template names and sandbox ids.

## Design

* Use the dynamic engine of `clap_complete` (`unstable-dynamic` feature,
  `CompleteEnv`). The shell script that the user loads is small and fixed.
  For each TAB, the shell starts airlock with `AIRLOCK_COMPLETE=<shell>`
  and the command line words. Airlock prints the candidates and exits.
  The static `generate()` API cannot complete run-time values.
* The variable is `AIRLOCK_COMPLETE`, not the default `COMPLETE`, so that it
  does not clash with other programs.
* The completion hook is the first step of `main`, before the panic hooks,
  the TLS provider, the argv split at `--` and the context. The request
  must be fast and must write only the candidates.
* `completions script <shell>` prints the registration script.
  `completions install [<shell>]` appends a line that evaluates it to the
  startup file: `~/.bashrc`, `$ZDOTDIR/.zshrc` or `~/.zshrc`, and
  `$XDG_CONFIG_HOME/fish/completions/airlock.fish` (fish loads it when
  the user completes an airlock command). The line checks that `airlock`
  is in `PATH`, so a removed airlock does not break the shell start.
* Install does nothing if the file matches
  `\bairlock\s+completions\s+script\s+<shell>\b`. Thus a second install
  does nothing, and the user can write the line in an own format.
* `clap_complete` recommends an eval at each shell start in place of a
  saved script, because the script and the binary must match. The install
  line follows that advice.
* The registration script calls the program as the user started it. A
  relative path becomes absolute. A bare name stays and resolves from
  `PATH`, so that an upgrade that changes the real binary path (for
  example Homebrew) does not break it.

## Value completers

* Template names and sandbox ids use `ArgValueCandidates`. The engine
  filters them by prefix.
* The completers load only the settings and the data dir layout. If the
  data dir or the database dir does not exist, they give no values. Thus a
  TAB never makes files.
* The registry read is async, and `main` is a tokio runtime. A nested
  `block_on` panics in a runtime thread, so the read runs on its own thread
  with a small `current_thread` runtime. The Rust test runs the completer
  in a `#[tokio::test]` to cover this.
* The registry list (`sandbox::list_entries`) reads the table in a read transaction and does not
  create it. Before, it opened the table with a write transaction, and a
  TAB could wait for the LMDB write lock while a sandbox started.
* LMDB does not allow two database opens at the same time in one
  process. Before, only write transactions opened databases, and the write
  lock kept them in order. Now a read transaction opens one too, so a
  process-wide mutex covers each transaction that opens a database.
* The help of a sandbox id is its project directory. zsh and fish show it.

## Notes

* The zsh script starts with a `compinit` call if `compdef` does not
  exist. Without it, the `compdef` line of the script fails at each shell
  start when the user has no `compinit` before the eval line.
* `unstable-dynamic` can change in a minor release of `clap_complete`.
  Check the completion bats tests after an update.
* After `--`, the command line belongs to the sandbox command. The engine
  gives no candidates there.
