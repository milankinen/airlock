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

## Forged upgrade test checks the upgrade check

The test for a 101 that a middleware forged used the script
`res.status = 101`. Scripts get only `req`, `log` and `env`, so `res`
was `nil` and the script failed. The test got its 502 from the script
error, so no test covered the refusal of a forged 101. The script now
sends the request, changes the upstream status to 101, and the test
checks that the 502 body is the one of the upgrade check ("upgrade not
accepted by upstream"). With the old script the new assert fails.

## Pack fingerprint from the non-default arg values

A pack version must change only for a breaking change: a change of the
result of an existing set of arg values. The install fingerprint covered
the name, the version and all arg values, defaults included. A new arg
added its default to the values, so the fingerprint of every sandbox
with the pack changed, and airlock reported the pack as changed, also
when nothing installs differently.

The fingerprint now covers the name, the version and only the arg values
that differ from their defaults (in key order, as a JSON object as
before). The setup script and `config.lua` still get all values with
the defaults. Results:

- An arg set to its default value gives the same fingerprint as no arg
  (the existing configure test already required this).
- A new arg with a default does not change the fingerprint. A new test
  adds a bool arg to a test pack and checks this.
- A change of a default value changes the result for users who did not
  set the arg, but not the fingerprint. So it is a breaking change and
  needs a new version.

No tag contains the packs yet, so the new formula needs no migration.
Development sandboxes with packs that have args see them as changed
one time.

## Mask injected secrets in response bodies

When an upstream echoed an injected secret in the response body, the
guest got the real value. Only response headers were masked, and a test
locked this in. We decided that this is a secret leak to the sandbox.

The response body now goes through a streaming masker after the
middleware chain:

- The masker holds back `longest - 1` bytes of the pending data. A match
  that starts before that tail fits completely in the data, so a value
  split across two chunks is found. At each position the longest real
  value wins, as in the header swap. A unit test pushes a text with
  nested secrets split at every byte and compares the result with the
  full-text masking.
- The body ends in the same poll as the inner body (`is_end_stream`), so
  hyper ends the message without one more poll.
- Trailers go through the header rewrite.
- A production surrogate has the length of its real value, so
  `Content-Length` and the size hint stay. If a pair differs in length,
  the response loses `Content-Length` and the size hint is unknown. The
  test constants first had a surrogate one byte longer than its real
  value, which showed this case (hyper cut the body at the old length).
- The masker cannot see into compressed bodies. When the target has
  injected secrets, the proxy sets `Accept-Encoding: identity` on the
  upstream request after middleware, so scripts cannot change it. A
  compressed body that still comes back gets a local 502, the same
  fail-closed rule as the API answer scan of the sign-in services.
- 101 responses and bodies that are already at their end (HEAD, 204,
  304) pass unchanged.

Known limits, also in the manual: no search for encoded forms (base64,
JSON escapes, URL encoding), and no check of WebSocket data after an
upgrade. Scripts still see the real values in `res:body()`.

The echo test now records what the upstream sent to check the unmasked
request, and checks that the guest gets surrogates in the body. New
tests cover a value split across streamed chunks (and the forced
`identity`) and the refusal of a compressed answer. The HTTPS test also
checks the body.

## Test: upstream close closes the guest connection

`http1_upstream_close_closes_guest_connection_without_502` checked only
that no 502 came back. A new test connection helper reads until the
proxy closes the guest connection and fails if it stays open. The test
uses it, so it now also checks the close.

## Test rename: unknown endpoint answer with token

`unknown_endpoint_fails_closed` checked only that a token in the answer
from an unknown host gives a 502. The request still goes upstream, which
is the intended behaviour (the answer scan is the guard). The name now
says what the test checks: `unknown_endpoint_answer_with_token_is_refused`.

## Test split: credential swap on other hosts

`credential_surrogates_are_swapped_on_every_api_path_only` also checked
the token host and the ChatGPT host, which the name did not say. Its
`platform.claude.com` check also read the log of the step before, which
worked only because that log was empty. The token and ChatGPT host checks
are now a separate test with their own logs, and both tests share a
grant helper.

## Test: exec that did not start keeps an existing record

`pack_whose_exec_did_not_start_keeps_its_record` ran on a new sandbox,
so there was no record to keep. It now stores a failed record of the
pack (with an old time) before the run, and checks that the record is
the same after it.

## Test: a file's own value wins over its own pack

`project_pack_overrides_user_files_and_its_own_file_overrides_pack` never
checked the second half of its name: when the local file set an image
next to its pack, `airlock.toml` also set one and hid the result. The
test now sets the local image first and checks that it wins over the
pack of the same file, then checks that `airlock.toml` wins over both.

## Test: home project found by `$HOME`

`home_project_is_found_by_env_home_or_password_database` tested only the
path compare and the password database, not the `$HOME` lookup. It now
sets `$HOME` to a temp directory (under the crate `HOME` lock) and runs
the full check on its `.airlock` and on a project below it.

## Test: clipboard grant without capability installs nothing

`grant_without_host_capability_installs_nothing` checked only that the
start succeeds. It now also checks that no FIFO and no shim exists at
the container rootfs paths of the bridge.

## Test: scan finds a token after a long run

`long_run_goes_out_in_parts_and_token_after_it_is_found` checked the
token with a new scanner, not after the long run. It now pushes the
token to the same scanner after the run, so the test checks that the
held-back state of a long run does not hide a later token.

## Test: minted surrogate has exactly 48 random bytes

`minted_surrogate_has_prefix_and_48_random_bytes` checked the length only
as a rough lower bound. It now strips the known prefix and checks that
the rest decodes from base64url to exactly 48 bytes.

## Test: request details scroll on Down

In `request_details_show_headers_and_follow_late_response`, the check
after `Down` passed whether or not the view scrolled: the details fit on
the screen, so nothing could scroll. The test now makes the terminal
short, presses `Down`, and checks that the path row of the same request
moved up by one line.

## Test rename: connection details follow the disconnect

`connection_details_follow_traffic_and_close_after_row_is_evicted` read
as if the details close by themselves when the row leaves the list. They
do not, and the test closes them with a click. The test is now
`connection_details_follow_traffic_and_disconnect_after_row_is_evicted`,
and its doc says that the details stay open until the user closes them.

## Test: transfer pair fits up to petabytes

`transfer_pair_fits_column_up_to_petabytes_and_is_truncated_beyond`
checked sizes only up to 1023 GB, though the name says petabytes. It now
also checks 1023 TB and 97 PB (99328 TB, the longest number that still
fits the 19-column pair).
