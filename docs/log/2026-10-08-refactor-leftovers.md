# Fixes for the refactor leftovers

The code and test comment reviews after the test suite rework
(`2026-10-08-test-suite-rework.md`, `2026-10-08-code-comment-rewrite.md`)
found bugs, weak tests and open decisions. They were listed in
`LEFTOVERS.md` on the `refactor-leftovers` branch. Each section below is
one commit.

## Error messages name the right setting and command

The vault backends told the user to set `settings.vault = "..."`. The
real key is `vault.storage` in the settings file, as the "vault is
disabled" message already says. The CA error told the user to run
`airlock up`, a command that does not exist. It now says `airlock start`.

## Remove a stale dead-code allow

The signal number function had an `allow(dead_code)` for a time when no
caller existed. The daemon start code calls it now, so the attribute
only hid future real dead code.

## Passthrough conflict check ignores case and a trailing dot

The check that refuses a passthrough target which overlaps a middleware,
inject or service target compared literal/literal and wildcard/wildcard
pairs as raw strings. Only the mixed pair went through the run-time host
matcher, which ignores case and one trailing dot. So `Example.com` or
`example.com.` gave no conflict with `example.com`, but at run time both
match the same host. Both patterns now go through the same canonical
form as the run-time matcher before the compare.

## Concurrent image pulls use their own layer download files

The registry pull and the `docker save` import downloaded each layer to
the fixed name `<key>.download.tmp` in the shared layer cache. The
registry pull also deleted that file before each pull. Two `airlock`
processes that pulled the same uncached image could write into one file
(corrupt tarball) or delete the file of the other process. The
layer-cache path already used a per-process name for this reason.

All three paths now get the name `<key>.download.<pid>.<seq>.tmp` from
one helper in the layer module. The rename to `<key>.download` stays the
commit. Because no later pull removes a unique name, the registry pull
now deletes its temp file itself when the pull or rename fails. The GC
sweep still removes `.tmp` files that a killed process left.

## Monitor tab click area matches the drawn tab

The tab bar drew 1 padding column before the first tab and 2 columns
between tabs. The click areas moved only 1 column between tabs. So a
click on the gap before the Monitor tab selected it, and a click on its
last column did nothing. Two constants now give the padding and the gap,
and both the drawing and the click areas use them. A new TUI test clicks
the gap column and the last tab column.

## UTF-8 mouse mode sends valid UTF-8

When a guest program enabled UTF-8 mouse mode (`\e[?1005h`), the encoder
used the default single-byte form. Cells at column or row 96 to 223 gave
raw bytes 0x80 to 0xFF, which are not valid UTF-8, so the program read
wrong positions. In mode 1005 each value (button, column, row, each plus
32) is now one UTF-8 character, as xterm and tmux do. Values of 128 or
more take 2 bytes, so the mode reaches coordinate 2015. The button value
also goes through UTF-8 encoding, because wheel events with modifiers
and motion can reach 128 or more.

## CPU box gives rows to the core bars first

The CPU box comment said that rows go to the core bars first, then the
load row, then the histogram. The code reserved up to 4 histogram rows
first, so a short box showed only 1 core bar and a histogram. We decided
that the core bars are the main content. The layout now gives rows to
the core bars, then the load row, then up to 4 histogram rows. In a
tall box the result is the same as before: extra rows go between the
core bars and the load row, so the load row and the histogram stay at
the bottom. A unit test draws the box at 3, 4 and 10 rows.

## Fake DNS IPs stay in their range

The virtual DNS server counted fake IPs up from `10.2.0.1` with no
limit. After 65,534 hostnames the IPs left `10.2.0.0/16`, which the doc,
the manual and the VM network setup comments assume. We chose reuse
over failure: a long sandbox session (for example a crawler) must not
lose DNS. After `10.2.255.254` the counter starts again at `10.2.0.1`.
The reused IP is always the oldest one, and the DNS answers have a TTL
of 5 minutes. So the guest has most likely forgotten its old hostname.
The old hostname is removed from both maps, and a later query for it
gets a new IP. A stale IP in the guest can then reach another hostname,
but the host still applies the network rules of that hostname, so this
is no policy bypass.

## `[env]` errors give exit code 2

The `[env]` error type says that `airlock start` reports it as a config
error (exit code 2), also when it comes from deep inside the project
setup. Only the early `[env]` check did that. The install config path
mapped the error to exit code 1, and the project open wrapped it in a
general error, which also gave exit code 1. The conversion from a
general error to an early exit now looks for the `[env]` error type in
the error chain and reports a config error. The install path maps the
error directly. We kept the type because the conversion now uses it.

## Encrypted vault keeps its Argon2 parameters on write

`load` derived the key with the `m`/`t`/`p` values in the file and
cached the key and the salt. `store` used the cached key and salt, but
wrote the built-in Argon2 parameters into the envelope. A vault file
from another release or tool opened one time, but after the first write
the file had parameters that did not match its key. The next process
could not decrypt it, also with the correct passphrase.

Of the two options (cache the parameters, or derive a new key with the
built-in parameters on write) we chose to cache them. A new key needs
the passphrase again, which a write in the middle of a run cannot always
ask for. The cache now holds the key with all KDF inputs (salt and
parameters). `store` writes them back unchanged, and `load` uses the
cached key only when all inputs in the file match. The parameter test
now also writes through the same handle and reads the file in a new
handle.

## One paste prints the clipboard once

A FIFO has no boundary between two pastes. The paste loop opened the
FIFO for writing, wrote the host clipboard, closed it and opened it
again at once. When the shim's `cat` had not yet seen end of file, it
still had the FIFO open. The new open then succeeded at once and wrote
the clipboard again into the same read. One `wl-paste` printed the
clipboard 2 to 6 times. Under parallel load the test failed in 71 of
160 runs.

Of the two options in the leftovers list, polling the write end for
`POLLHUP` does not work: `cat` reads until end of file, and it gets end
of file only after the write end closes. A request/response protocol
needs `mkfifo` or other tools in the image. We chose a third way that
needs nothing in the image: when a reader has opened the FIFO, the loop
puts a new FIFO with the same owner at the same path (make it next to
the path, then rename over it). The reader keeps the old FIFO, but no
new reader can open it, so the close of the write end always gives it
end of file. The next paste opens the new FIFO. With the fix, 0 of 160
parallel runs failed, and the test is no longer ignored.

Two pastes that open the FIFO in the short time between the open and
the rename still share one stream, as before.

## Service stop test uses a Unix socket

The test of the service stop order bound a TCP listener on
`127.0.0.1:0`, stopped the services and bound the same port again. Many
parallel tests bind ephemeral ports, and the kernel could give the freed
port to one of them first (1 failure in 11 full runs). The test only
needs to see that the service listener closed. It now listens on a Unix
socket in a private temp directory and checks that a connect is
refused after the stop. No other test can take that socket.
