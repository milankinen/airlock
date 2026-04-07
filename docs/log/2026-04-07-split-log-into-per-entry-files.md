# Split development log into per-entry files

Replaced the single `docs/LOG.md` with individual files under
`docs/log/<yyyy-mm-dd>-<title>.md` — one file per log entry.

45 entries from the original log were split out. Updated `CLAUDE.md`
to document the new convention so future entries are created as
separate files rather than prepended to a combined log.

Motivation: the combined LOG.md was a frequent merge conflict source
since all contributors prepend to the same file.
