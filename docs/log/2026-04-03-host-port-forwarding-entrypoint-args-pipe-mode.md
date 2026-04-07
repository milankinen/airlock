# Host port forwarding, entrypoint args, pipe mode

### Host port forwarding

Expose configured host ports (`config.network.host_ports`) to the VM.
Per-port iptables REDIRECT rules forward specific localhost ports to
the host via the proxy, while other localhost traffic passes through
directly so local VM services work. Host ports are passed via kernel
cmdline (`ezpez.host_ports=9999,8080`) and enforced both in iptables
and in the CLI's NetworkProxyImpl.

### Entrypoint args

`ez -- <args>` overrides the entire OCI command. User args replace
the image's entrypoint+cmd entirely (e.g., `ez -- ls /usr`). When
no args are provided, falls back to image entrypoint+cmd or `/bin/sh`.

### Pipe mode (no PTY)

When stdin is not a TTY (piped input), the VM runs without PTY:

- OCI config sets `"terminal": false`
- Supervisor spawns process with piped stdin/stdout/stderr instead of
  PTY, giving proper separation of stdout and stderr
- CLI skips raw terminal mode and resize handling
- StdinImpl handles optional resize signal (None in pipe mode)

This enables: `echo data | ez -- grep pattern`,
`ez -- sh -c 'echo hi; exit 42'` (exit codes propagate).
