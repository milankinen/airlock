# Tighten Lua middleware instruction limit to 100k

The HTTP middleware sandbox installed a Lua instruction hook every
1,000,000 instructions. On a typical x86_64 machine that's tens of
milliseconds to low hundreds of milliseconds of CPU per trigger —
enough wall-clock that a pathological script can noticeably stall a
request before the hook finally fires and aborts it.

Middleware scripts are user-authored (not guest-authored), so this
isn't a security boundary — a user who wants to DoS their own sandbox
has easier ways. But the review flagged it as a sandboxing-hygiene
point, and the looser limit provides no benefit: 100k instructions is
still plenty for any realistic header-rewrite, JSON-patch, or
token-injection script, while giving a 10× shorter worst-case stall
for runaway loops.

Dropped to `every_nth_instruction(100_000)`. No wall-clock deadline
added — the instruction hook is sufficient, and a separate
`Instant::now()` check per hook would either duplicate the work or
need to share state with the script (timers, accumulated time) that
we don't need.
