# Fix clipboard copy test race

`copy_through_every_tool_name_reaches_host_clipboard` failed on CI with
"timed out: three copies".

## Cause

The copy loop opens the copy FIFO, then renews it (new FIFO at the same
path). The test ran the shims one after the other. A shim exits as soon
as its write ends. If the next shim opened the path before the renew, it
got the old FIFO, which the loop still read. The two payloads mixed into
one copy, so the host got two copies and not three.

`write_fifo_cycle` already handled this race for raw FIFO writes. The
shim runs did not.

## Fix

A new test helper runs a shim as one FIFO operation and waits until the
FIFO inode changes (the renew happened). `write_fifo_cycle` and the new
helper share one generic wait. The copy tests that run more than one shim
use the new helper.

Paste tests do not have this race: the paste loop renews the FIFO before
it writes, and the shim exits only after the write end closes.

Production keeps the known small window: two copies that start within
microseconds of each other can still merge.
