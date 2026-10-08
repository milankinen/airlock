use crossterm::event::{KeyCode, KeyModifiers};

use crate::keys::Action;
use crate::test_cfg::{Tui, request, request_with};
use crate::{StatsSnapshot, TuiSettings};

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

#[test]
fn guest_stats_show_in_monitor_panels_and_status_bar() {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    assert!(tui.screen().contains("awaiting stats…"));

    tui.stats(StatsSnapshot {
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

#[test]
fn status_bar_counts_allowed_and_denied_requests_on_any_tab() {
    let mut tui = Tui::new();
    tui.network_event(request(1, "GET", "/", "a.example"));
    tui.network_event(request(2, "GET", "/", "a.example"));
    tui.network_event(request_with(3, "GET", "/", "b.example", false, &[]));

    assert!(tui.row(29).contains("Network 2 1"));
}
