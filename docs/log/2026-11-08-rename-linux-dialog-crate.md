# Rename airlock-dialog crate to airlock-linux-dialog

The details window crate works only on Linux (X11). The macOS build
uses native dialogs and does not depend on it. The new name shows the
platform, the same as `airlock-macos-notifier`.

Changes:

* Crate dir `app/airlock-dialog` -> `app/airlock-linux-dialog`
* Package name and Rust path `airlock_dialog` -> `airlock_linux_dialog`
* Manual page `technical/approvals.md` uses the new name

Old log entries keep the old name.
