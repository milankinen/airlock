# Short CLI messages with uniform case

The user texts of the CLI were long and their case was mixed. Some
errors were 100 to 470 characters long, or took many lines. Some
errors started uppercase ("No sandbox for ..."), and some status lines
started lowercase ("stopped the sandbox").

Length:

* Target is 80 to 100 characters, without the inserted values.
  A message tells the problem and, when it helps, the fix.
* The plaintext vault warning and the disabled-vault help are now one
  line each. They no longer list all `vault.storage` values with
  comments.
* Merged messages: the absolute, `~` and `..` mask path errors are now
  `mask_path_not_relative`. The two no-TTY vault errors are now
  `no_tty_for_passphrase`.
* `services_unavailable` has one form for one or many services, so
  the caller no longer picks singular or plural words.
* `path_leaves_writable_dir` no longer names the writable dir. The
  real path is enough for the fix.

Case rule:

* Errors and warnings start lowercase. They join into chains such as
  `context: cause` and come after prefixes such as `airlock: `.
  Follow-up parts join with a comma, "and", "because" or brackets,
  not with a new sentence. They have no final period.
* Prompts, choices, titles, notes, tips, status, progress and success
  lines start uppercase and can have many sentences.
* Exceptions: text that starts with a placeholder, a lowercase name
  (`airlock`, `libkrun`), a config key, a flag or a path. Also key
  help lines, short progress labels (`cached`, `ready`) and form or
  table labels.
* Acronyms (`VM`, `TTY`, `HTTP`) stay uppercase.

The ACP login hint now joins the text of the agent with a comma, for
example `Authentication required, log in inside the sandbox, ...`.
