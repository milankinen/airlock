//! Monitor tab: tab changes, key bindings, sandbox kill, statistics and the
//! status bar.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::keys::Action;
use crate::test_cfg::{Tui, request, request_with};
use crate::{StatsSnapshot, TuiSettings};

/// Test that the tab keys change between the Sandbox and Monitor tabs, and
/// that no tab key goes to the sandbox.
///   1. Send output, open the Monitor tab and check its content
///   2. Press the back key and check that the Sandbox tab shows
///   3. Open the Monitor tab, press F1 and check that the Sandbox tab shows
///   4. Check that the sandbox got no input
#[test]
fn switching_tabs_with_keys_shows_monitor_and_back_returns_to_sandbox() {
    let mut tui = Tui::new();
    tui.output(b"guest prompt");

    tui.key(KeyCode::F(2));
    let screen = tui.screen();
    assert!(
        screen.contains("airlock sandbox monitor (1.2.3)"),
        "{screen}"
    );
    assert!(screen.contains("/work/project"));
    assert!(screen.contains("No HTTP requests observed yet."));
    assert!(!screen.contains("guest prompt"));

    tui.key(KeyCode::Char('q'));
    assert_eq!(tui.row(0), "guest prompt");

    tui.key(KeyCode::F(2));
    tui.key(KeyCode::F(1));
    assert_eq!(tui.row(0), "guest prompt");
    assert!(tui.sent().is_empty());
}

/// Test that Ctrl+D on the Monitor tab sends SIGHUP and then SIGTERM to the
/// sandbox process. An idle shell ignores SIGTERM but exits on SIGHUP.
///   1. Press Ctrl+D on the Sandbox tab and check that it goes to the sandbox
///      as a byte, not as signals
///   2. Press Ctrl+D on the Monitor tab and check the signals 1 and 15
///   3. Open the details of a request, press Ctrl+D and check the signals
///      again
#[test]
fn ctrl_d_on_monitor_tab_sends_hangup_then_terminate() {
    let mut tui = Tui::new();
    tui.key_with(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(tui.sent_bytes(), b"\x04");
    assert!(tui.signals().is_empty());

    tui.key(KeyCode::F(2));
    tui.key_with(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(tui.signals(), [1, 15]);

    tui.network_event(request(1, "GET", "/", "example.com"));
    tui.key(KeyCode::Enter);
    tui.key_with(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(tui.signals(), [1, 15]);
}

/// Test that custom key bindings change the tab bar labels and the tab
/// changes.
///   1. Bind Ctrl+M to the Monitor tab and `b` to back
///   2. Check that the tab bar shows the Ctrl+M label
///   3. Press Ctrl+M and check that the Monitor tab shows
///   4. Press `b` and check that the Monitor tab closes
#[test]
fn rebound_tab_keys_drive_tab_bar_labels_and_switching() {
    let mut settings = TuiSettings::default();
    settings.keys.bind(Action::SwitchMonitor, ["ctrl+m", "f2"]);
    settings.keys.bind(Action::Back, ["b"]);
    let mut tui = Tui::with(120, 30, settings);
    assert!(tui.row(29).contains("Ctrl+M Monitor"));

    tui.key_with(KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert!(tui.screen().contains("airlock sandbox monitor"));

    tui.key(KeyCode::Char('b'));
    assert!(!tui.screen().contains("airlock sandbox monitor"));
}

/// Test that sandbox statistics show in the Monitor panels and in the status
/// bar.
///   1. Open the Monitor tab and check the text for missing statistics
///   2. Send a snapshot with two cores, 512 MiB used of 2 GiB, and a load
///   3. Check the CPU, load and memory values in the panels and status bar
#[test]
fn guest_stats_show_in_monitor_panels_and_status_bar() {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    assert!(tui.screen().contains("awaiting stats…"));

    tui.stats(StatsSnapshot {
        // The total CPU use is the average of the cores: 75%.
        per_core: vec![50, 100],
        total_bytes: 2 * 1024 * 1024 * 1024,
        used_bytes: 512 * 1024 * 1024,
        load_avg: (0.5, 0.25, 1.5),
    });

    let screen = tui.screen();
    assert!(!screen.contains("awaiting stats…"), "{screen}");
    assert!(tui.row_with(" cpu ").contains("75%"));
    assert!(tui.row_with("c1 ").contains("100%"));
    assert!(screen.contains("load 0.50 0.25 1.50"));
    assert!(tui.row_with(" memory ").contains("25%"));
    assert!(tui.row_with(" total ").contains("2.0 GiB"));
    assert!(tui.row_with(" used ").contains("512 MiB"));
    let status = tui.row(29);
    assert!(status.contains("CPU 75%"), "{status}");
    assert!(status.contains("Memory 512 MiB / 2.0 GiB"), "{status}");
}

/// Test that the status bar counts allowed and denied requests, also while
/// the Sandbox tab is open.
///   1. Send two allowed requests and one denied request
///   2. Check that the status bar shows 2 allowed and 1 denied
#[test]
fn status_bar_counts_allowed_and_denied_requests_on_any_tab() {
    let mut tui = Tui::new();
    tui.network_event(request(1, "GET", "/", "a.example"));
    tui.network_event(request(2, "GET", "/", "a.example"));
    tui.network_event(request_with(3, "GET", "/", "b.example", false, &[]));

    assert!(tui.row(29).contains("Network 2 1"));
}
