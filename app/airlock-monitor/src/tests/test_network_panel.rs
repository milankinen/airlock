//! Network panel on the Monitor tab: request and connection lists, their
//! details, selection, scroll and counts.

use crossterm::event::{KeyCode, MouseEventKind};

use crate::TuiSettings;
use crate::test_cfg::{
    Tui, connect, disconnect, request, request_with, response, response_with, traffic,
};

/// Make a 120x30 TUI with default settings and the Monitor tab open.
fn monitor() -> Tui {
    monitor_with(120, 30, TuiSettings::default())
}

/// Make a TUI with the given size and settings and the Monitor tab open.
fn monitor_with(cols: u16, rows: u16, settings: TuiSettings) -> Tui {
    let mut tui = Tui::with(cols, rows, settings);
    tui.key(KeyCode::F(2));
    tui
}

/// Return the row where `text` first shows. Panics if it does not show.
fn y_of(tui: &mut Tui, text: &str) -> u16 {
    tui.find(text)
        .unwrap_or_else(|| panic!("{text:?} not on screen"))
        .1
}

/// Test that the request list shows the newest request first, with its
/// target and policy result, and counts allowed and denied requests.
///   1. Send an allowed request, then a denied request
///   2. Check the target and result of each row
///   3. Check that the denied request is above the allowed request
///   4. Check the counts 1 allowed and 1 denied
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

/// Test that a deny from a middleware on the response changes an allowed
/// request to denied. A 403 from the upstream server is not a deny.
///   1. Send two allowed requests and check the counts
///   2. Send a 403 response from upstream and a 403 response from a
///      middleware deny
///   3. Check that only the second request is denied, also in the counts
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

/// Test that a middleware deny for a request that left the list still
/// changes the counts. The counts must agree with the policy results.
///   1. Limit the list to one request and send two requests
///   2. Send a middleware deny for the first request, which left the list
///   3. Check that the kept request is allowed and the counts show 1 denied
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

/// Test that the request details show the request headers and change when a
/// response comes after the details opened.
///   1. Send two requests, select the older request and open its details
///   2. Check the path, the headers, the missing response and the select hint
///   3. Send the response and check its status and headers
///   4. Make the terminal short, press Down and check that the details of
///      the same request scroll by one line
///   5. Close and open the details again and check that the response stays
///   6. Send a late middleware deny and check that the status is Denied
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

    // In the details, Down scrolls the body by one line. It does not select
    // a different request. The short terminal makes the body taller than
    // the panel, so that it can scroll.
    tui.resize(120, 20);
    let path_y = y_of(&mut tui, "/v1/models");
    tui.key(KeyCode::Down);
    assert_eq!(y_of(&mut tui, "/v1/models"), path_y - 1);
    tui.resize(120, 30);

    tui.key(KeyCode::Esc);
    let screen = tui.screen();
    assert!(!screen.contains("Request details"));
    assert!(screen.contains("/other"));

    tui.key(KeyCode::Enter);
    assert!(tui.screen().contains("404 Not Found"));

    tui.network_event(response(1, 403, true));
    assert!(tui.row_with("Status").contains("Denied"));
}

/// Test that the selection keys and the mouse wheel select the row that
/// Enter opens.
///   1. Send three requests
///   2. Move the selection with End, Up, Home, PageDown, PageUp and the wheel
///   3. After each move, open the details and check the path
#[test]
fn selection_keys_pick_row_that_enter_opens() {
    let mut tui = monitor();
    for (id, path) in [(1, "/oldest"), (2, "/middle"), (3, "/newest")] {
        tui.network_event(request(id, "GET", path, "a.example"));
    }
    let open_path = |tui: &mut Tui| {
        tui.key(KeyCode::Enter);
        let row = tui.row_with("Path");
        // `x` is a cancel key. It closes the details.
        tui.key(KeyCode::Char('x'));
        row
    };

    assert!(open_path(&mut tui).contains("/newest"));
    tui.key(KeyCode::End);
    assert!(open_path(&mut tui).contains("/oldest"));
    tui.key(KeyCode::Up);
    assert!(open_path(&mut tui).contains("/middle"));
    // A page is larger than the list, so the page keys go to the ends. A
    // wheel step moves three rows, which also goes to the ends.
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

/// Test that a new request does not move the selection from its request,
/// but the selection follows new requests when the newest row is selected.
///   1. Select the older of two requests, send a new request, and check that
///      Enter opens the older request
///   2. Select the newest row, send a new request, and check that Enter
///      opens the new request
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

/// Test that the connection list shows the latest traffic total of each
/// connection, and that the sub-tabs change with keys and clicks.
///   1. Send three connections and three traffic totals for one connection
///   2. Open the Connections sub-tab and check the traffic, the denied
///      connection and the counts
///   3. Change the sub-tab with Tab and with clicks on the sub-tab labels
#[test]
fn connections_show_latest_cumulative_traffic_per_connection() {
    let mut tui = monitor_with(160, 30, TuiSettings::default());
    tui.network_event(connect(1, "quiet.example", true));
    tui.network_event(connect(2, "busy.example", true));
    tui.network_event(connect(3, "blocked.example", false));
    // Traffic events carry running totals, so the row must show the last
    // value (400 B up, 6.3 GB down) and not a sum.
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

/// Test that open connection details continue to show traffic and the close
/// of their connection after the row left the list.
///   1. Limit the list to one connection and open the details of a
///      connection
///   2. Send a second connection, then traffic and a disconnect for the
///      first connection
///   3. Check the bytes and the closed state in the details
///   4. Close the details and check that the second row has no traffic
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

/// Test that the details scroll through a long header list, and that the
/// scroll stops at the end of the content.
///   1. Open the details of a request with 40 headers on a small terminal
///   2. Go to the bottom, scroll down two more lines, then up one line
///   3. Check that the last line is hidden, so the scroll stopped at the end
///   4. Check that Home goes to the top and a wheel step scrolls three lines
///   5. Close and open the details and check that they start at the top
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

    // A wheel step scrolls three lines. This hides the empty line, Status
    // and Received, but Method stays at the top.
    tui.mouse(MouseEventKind::ScrollDown, (10, 10));
    assert!(!tui.screen().contains("Received"));
    assert!(tui.screen().contains("Method"));

    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Enter);
    assert!(tui.screen().contains("Method"));
}
