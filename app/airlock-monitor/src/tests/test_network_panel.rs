use crossterm::event::{KeyCode, MouseEventKind};

use crate::TuiSettings;
use crate::test_cfg::{
    Tui, connect, disconnect, request, request_with, response, response_with, traffic,
};

fn monitor() -> Tui {
    monitor_with(120, 30, TuiSettings::default())
}

fn monitor_with(cols: u16, rows: u16, settings: TuiSettings) -> Tui {
    let mut tui = Tui::with(cols, rows, settings);
    tui.key(KeyCode::F(2));
    tui
}

fn y_of(tui: &mut Tui, text: &str) -> u16 {
    tui.find(text)
        .unwrap_or_else(|| panic!("{text:?} not on screen"))
        .1
}

#[test]
fn requests_list_newest_first_with_policy_result_and_counts() {
    let mut tui = monitor();
    tui.network_event(request(1, "GET", "/first", "api.example.com"));
    tui.network_event(request_with(
        2,
        "POST",
        "/second",
        "evil.example",
        false,
        &[],
    ));

    let first = tui.row_with("GET /first");
    let second = tui.row_with("POST /second");
    assert!(first.contains("api.example.com:443") && first.contains("Allowed"));
    assert!(second.contains("evil.example:443") && second.contains("Denied"));
    assert!(y_of(&mut tui, "POST /second") < y_of(&mut tui, "GET /first"));
    assert!(tui.row_with(" allowed ").contains("1 allowed  1 denied"));
}

#[test]
fn middleware_deny_on_response_overturns_allowed_request() {
    let mut tui = monitor();
    tui.network_event(request(1, "GET", "/upstream-403", "a.example"));
    tui.network_event(request(2, "GET", "/middleware-deny", "a.example"));
    assert!(tui.row_with(" allowed ").contains("2 allowed  0 denied"));

    tui.network_event(response(1, 403, false));
    tui.network_event(response(2, 403, true));

    assert!(tui.row_with("/upstream-403").contains("Allowed"));
    assert!(tui.row_with("/middleware-deny").contains("Denied"));
    assert!(tui.row_with(" allowed ").contains("1 allowed  1 denied"));
}

#[test]
fn middleware_deny_for_evicted_request_still_moves_count() {
    let settings = TuiSettings {
        max_http_requests: 1,
        ..TuiSettings::default()
    };
    let mut tui = monitor_with(120, 30, settings);
    tui.network_event(request(1, "GET", "/evicted", "a.example"));
    tui.network_event(request(2, "GET", "/kept", "a.example"));

    tui.network_event(response(1, 403, true));

    assert!(!tui.screen().contains("/evicted"));
    assert!(tui.row_with("/kept").contains("Allowed"));
    assert!(tui.row_with(" allowed ").contains("1 allowed  1 denied"));
}

#[test]
fn request_details_show_headers_and_follow_late_response() {
    let mut tui = monitor();
    tui.network_event(request_with(
        1,
        "GET",
        "/v1/models",
        "api.example.com",
        true,
        &[("accept", "application/json")],
    ));
    tui.network_event(request(2, "GET", "/other", "api.example.com"));
    tui.key(KeyCode::Down);
    tui.key(KeyCode::Enter);

    let screen = tui.screen();
    assert!(screen.contains("Request details"), "{screen}");
    assert!(tui.row_with("Path").contains("/v1/models"));
    assert!(screen.contains("accept: application/json"));
    assert!(screen.contains("(no response yet)"));
    assert!(screen.contains("click to select text"));

    tui.network_event(response_with(1, 404, false, &[("server", "nginx")]));
    let screen = tui.screen();
    assert!(screen.contains("404 Not Found"), "{screen}");
    assert!(screen.contains("server: nginx"));

    tui.key(KeyCode::Down);
    assert!(tui.screen().contains("Method"));

    tui.key(KeyCode::Esc);
    let screen = tui.screen();
    assert!(!screen.contains("Request details"));
    assert!(screen.contains("/other"));

    tui.key(KeyCode::Enter);
    assert!(tui.screen().contains("404 Not Found"));

    tui.network_event(response(1, 403, true));
    assert!(tui.row_with("Status").contains("Denied"));
}

#[test]
fn selection_keys_pick_row_that_enter_opens() {
    let mut tui = monitor();
    for (id, path) in [(1, "/oldest"), (2, "/middle"), (3, "/newest")] {
        tui.network_event(request(id, "GET", path, "a.example"));
    }
    let open_path = |tui: &mut Tui| {
        tui.key(KeyCode::Enter);
        let row = tui.row_with("Path");
        tui.key(KeyCode::Char('x'));
        row
    };

    assert!(open_path(&mut tui).contains("/newest"));
    tui.key(KeyCode::End);
    assert!(open_path(&mut tui).contains("/oldest"));
    tui.key(KeyCode::Up);
    assert!(open_path(&mut tui).contains("/middle"));
    tui.key(KeyCode::Home);
    tui.key(KeyCode::PageDown);
    assert!(open_path(&mut tui).contains("/oldest"));
    tui.key(KeyCode::PageUp);
    assert!(open_path(&mut tui).contains("/newest"));
    tui.mouse(MouseEventKind::ScrollDown, (10, 10));
    assert!(open_path(&mut tui).contains("/oldest"));
    tui.mouse(MouseEventKind::ScrollUp, (10, 10));
    assert!(open_path(&mut tui).contains("/newest"));
}

#[test]
fn new_request_keeps_selection_on_same_entry_unless_following_newest() {
    let mut tui = monitor();
    tui.network_event(request(1, "GET", "/one", "a.example"));
    tui.network_event(request(2, "GET", "/two", "a.example"));
    tui.key(KeyCode::Down);
    tui.network_event(request(3, "GET", "/three", "a.example"));
    tui.key(KeyCode::Enter);
    assert!(tui.row_with("Path").contains("/one"));

    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Home);
    tui.network_event(request(4, "GET", "/four", "a.example"));
    tui.key(KeyCode::Enter);
    assert!(tui.row_with("Path").contains("/four"));
}

#[test]
fn connections_show_latest_cumulative_traffic_per_connection() {
    let mut tui = monitor_with(160, 30, TuiSettings::default());
    tui.network_event(connect(1, "quiet.example", true));
    tui.network_event(connect(2, "busy.example", true));
    tui.network_event(connect(3, "blocked.example", false));
    for up in [10, 90, 400] {
        tui.network_event(traffic(2, up, 6 * 1024 * 1024 * 1024 + 300 * 1024 * 1024));
    }
    tui.network_event(disconnect(1));

    tui.key(KeyCode::Char('c'));

    let screen = tui.screen();
    assert!(screen.contains("Transferred"), "{screen}");
    assert!(tui.row_with("busy.example").contains("↑ 400B ↓ 6.3GB"));
    assert!(!tui.row_with("quiet.example").contains('↑'));
    assert!(tui.row_with("blocked.example").contains("Denied"));
    assert!(tui.row_with(" allowed ").contains("2 allowed  1 denied"));

    tui.key(KeyCode::Tab);
    assert!(tui.screen().contains("No HTTP requests observed yet."));
    tui.click_text("Connections");
    assert!(tui.screen().contains("busy.example"));
    tui.click_text("Requests");
    assert!(tui.screen().contains("No HTTP requests observed yet."));
}

#[test]
fn connection_details_follow_traffic_and_close_after_row_is_evicted() {
    let settings = TuiSettings {
        max_tcp_connections: 1,
        ..TuiSettings::default()
    };
    let mut tui = monitor_with(120, 30, settings);
    tui.network_event(connect(1, "first.example", true));
    tui.key(KeyCode::Char('c'));
    tui.key(KeyCode::Enter);
    assert!(tui.row_with("State").contains("Open"));

    tui.network_event(connect(2, "second.example", true));
    tui.network_event(traffic(1, 42, 84));
    tui.network_event(disconnect(1));

    assert!(tui.row_with("Sent").contains("42B (42 bytes)"));
    assert!(tui.row_with("Received").contains("84B (84 bytes)"));
    assert!(tui.row_with("State").contains("Closed"));
    assert!(tui.screen().contains("Disconnected"));

    tui.click_text("×");
    let row = tui.row_with("second.example");
    assert!(!row.contains('↑'), "{row}");
}

#[test]
fn details_scroll_through_long_headers_clamped_to_content() {
    let headers: Vec<(String, String)> = (0..40)
        .map(|i| (format!("x-header-{i:02}"), "v".to_string()))
        .collect();
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut tui = monitor_with(120, 24, TuiSettings::default());
    tui.network_event(request_with(1, "GET", "/", "a.example", true, &headers));
    tui.key(KeyCode::Enter);
    let top = tui.screen();
    assert!(top.contains("Method"), "{top}");
    assert!(!top.contains("x-header-39"));

    tui.key(KeyCode::End);
    let bottom = tui.screen();
    assert!(bottom.contains("(no response yet)"));
    assert!(!bottom.contains("Method"));

    tui.key(KeyCode::Down);
    tui.key(KeyCode::Down);
    tui.key(KeyCode::Up);
    assert!(!tui.screen().contains("(no response yet)"));

    tui.key(KeyCode::Home);
    assert!(tui.screen().contains("Method"));

    tui.mouse(MouseEventKind::ScrollDown, (10, 10));
    assert!(!tui.screen().contains("Received"));
    assert!(tui.screen().contains("Method"));

    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Enter);
    assert!(tui.screen().contains("Method"));
}
