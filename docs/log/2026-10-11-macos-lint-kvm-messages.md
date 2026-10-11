# Fix macOS lint for KVM help status messages

Moving all CLI texts to `messages.rs` made the KVM help status texts
plain `pub` items in `messages::cli`. Only the Linux variant of
`cli::platform_status` uses them. On macOS, `mise lint` failed with
`dead_code` for `KVM_ACCESS_GRANTED`, `KVM_NOT_AVAILABLE`,
`KVM_PERMISSION_DENIED`, `kvm_unavailable` and `HELP_STATUS_TITLE`.

Fix: gate each item with `#[cfg(target_os = "linux")]`, the same as
the other platform-specific texts in `messages.rs`. This is better
than `#[allow(dead_code)]` because the compiler still finds texts that
become unused on Linux.
