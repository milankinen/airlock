# One exit code for user aborts in start

`airlock start` gave different exit codes for a user abort. An `Abort`
choice or Esc in a start question gave 0, and Ctrl+C gave 130. Some
aborts were plain errors: an aborted vault passphrase prompt gave 2
(config error), and an aborted registry sign-in gave 1. Ctrl+C during
docker save, layer fetch or the auth retry also gave 1. A script could
not tell an abort from a success or a failure.

Now all user aborts exit with 130. Esc and `Abort` print "Aborted.".
Ctrl+C prints nothing. The wizard abort keeps its own message. An abort
with a failed wizard check stays a config error (2). `templates create`
shares the start exit rules, so its aborts also give 130. The prompt
texts of the vault and sign-in aborts do not show in start any more.
The generic "Aborted." replaces them.

The prompts that have no answer on Esc (vault passphrase, registry
sign-in) now return a typed abort error. The start exit conversion
finds it, or a Ctrl+C prompt error, anywhere in the error chain. It
keeps the two kinds apart. In raw mode, Ctrl+C is a key press and does
not set the process interrupt flag, so the kind decides if the message
shows. The `[env]` errors keep only the error text, so they record the
abort kind. Thus the abort survives the env resolve. The oci Ctrl+C
paths now return the same typed interrupt as the image pull.

The vault now records an aborted unlock. Before, a secret lookup hid
the open error, and each later lookup asked for the passphrase again.
One Esc showed the prompt up to four times for two `[env]` entries. A
correct passphrase on the second prompt also gave a false "missing
variable" error. Later reads and writes now fail with the recorded
abort. The setup wizard clears the record before each check, so the
user can abort the prompt of a check and try again. Optional
`[env]` entries and the saved registry credentials lookup used to treat
an aborted unlock as "no value" and continue. Now they end the run.

The exit codes of other commands do not change.
