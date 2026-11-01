//! Helper app that asks the user to allow or deny a temporary network grant.
//!
//! Notification Center accepts notifications only from an app with a bundle
//! identity. Thus airlock starts this program inside a small app bundle. The
//! program reads one request line from stdin, shows a notification with
//! "Allow" and "Deny" buttons and writes one answer line to stdout. The
//! program stops when the time limit ends or when airlock closes stdin.
//! When the request has details, the notification can open a window that
//! shows them with the same buttons.
//! After an allow answer, the program can keep a notice with an "Abort"
//! button until airlock closes stdin, and write one abort event line.
//!
//! Other hosts do not have Notification Center. There the program writes an
//! error answer and stops.

#[cfg(target_os = "macos")]
mod notifier_server;
mod protocol;

#[cfg(target_os = "macos")]
fn main() {
    notifier_server::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    use protocol::{ChildAnswer, ChildDecision};

    println!(
        "{}",
        ChildAnswer::new(ChildDecision::Error, "only macOS").encode()
    );
    std::process::exit(1);
}
