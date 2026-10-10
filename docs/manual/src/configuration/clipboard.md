# Clipboard

Programs inside the sandbox have no clipboard of their own. Copying from
an editor or a tool running in the VM does nothing, usually without any
error to explain why.

The `[clipboard]` section connects the sandbox to your real clipboard.
Once it is on, copying inside the sandbox works the way you would expect
— the text lands on your system clipboard and you can paste it anywhere.

You turn on copying and pasting separately, and both are off by
default. Each one opens a small hole in the sandbox, so read the security
section before you enable either.

## Turning it on

```toml
[clipboard]
copy = true
```

Copy something inside the sandbox and it is now on your clipboard.
Nothing inside the sandbox can read your clipboard back until you also
turn on `paste`.

## Options

```toml
[clipboard]
copy = false    # default; sandbox → your clipboard
paste = false    # default; your clipboard → sandbox
copy_limit = "1 MB"   # default; largest single copy
```

`copy_limit` uses the same size format as `vm.memory` (`"512 KB"`,
`"2 MB"`). airlock discards a copy larger than the limit and leaves your
clipboard intact.

## Security

Think of these as two separate permissions, because they carry very
different risks.

**`copy` lets the sandbox put text on your clipboard.** The danger is
not the amount of text but what you might do with it: you could later
paste text copied out of the sandbox into a terminal, an editor, or a
chat window, where it may do something you did not intend.

**`paste` lets the sandbox read your clipboard whenever it likes.** This
is the one to think hardest about. It is not limited to text you
deliberately paste in — a program in the sandbox can look at your
clipboard at any moment, without you doing anything. If you last copied
a password, an API key, or a customer's details, that is what it sees.
Turn it on only when you **really** need it, and consider turning it off
again afterwards.

airlock enforces both permissions outside the sandbox, so a compromised
program inside it cannot grant itself access it was not given. A
setting you left off is genuinely unavailable rather than merely
discouraged.

## If it isn't working

**Nothing happens when you copy.** airlock needs a working clipboard on
your own machine to hand things to. On macOS this is always available.
On Linux it means a running desktop session — over a bare SSH connection
there is no clipboard to reach, and airlock will say so at startup and
continue without the bridge.

**One particular app still doesn't copy.** Some programs decide for
themselves whether a clipboard exists before they try to use one, and
conclude there isn't one inside a VM. airlock does not pretend otherwise,
because that would mislead everything else running in the sandbox. You
can tell such a program what it expects to see:

```toml
[clipboard]
copy = true

[env]
WAYLAND_DISPLAY = "airlock-0"
```

airlock never uses the value itself. For example, Claude Code needs this
for copying. Pasting works without it. The
[`claude` pack](../packs/agents/claude-code.md#clipboard) sets this
variable for you.

**Still nothing.** A few applications handle the clipboard entirely
internally rather than through the system, and airlock cannot bridge
those. This is uncommon outside graphical apps, which would not run in
the sandbox anyway.
