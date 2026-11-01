//! Approval dialog from the host: answer, modal input, details, scroll and
//! queue.

use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEventKind,
};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use crate::app::Tab;
use crate::test_cfg::Tui;
use crate::{ApprovalDetail, ApprovalDiffKind, ApprovalDiffLine, ApprovalRequest, TuiEvent};

/// Body text of each test request.
const BODY: &str = "The sandbox asks for temporary access to api.example.com:443.";

/// Send a request with `title` and `details` to the TUI.
/// Returns:
///   The receiver that gets the answer.
fn ask(tui: &mut Tui, title: &str, details: Vec<ApprovalDetail>) -> oneshot::Receiver<bool> {
    let (reply, rx) = oneshot::channel();
    tui.send(TuiEvent::Approval(ApprovalRequest {
        title: title.into(),
        body: BODY.into(),
        details,
        reply,
    }));
    rx
}

/// A detail with a plain or a code value.
fn detail(label: &str, value: &str, mono: bool) -> ApprovalDetail {
    ApprovalDetail {
        label: label.into(),
        value: value.into(),
        mono,
        diff: None,
    }
}

/// Details of a rule change: a diff, a wide command and a long TOML value.
/// The command ends with `WIDE-END` and the TOML ends with `LAST-LINE`.
fn rule_change_details() -> Vec<ApprovalDetail> {
    let diff = [
        (ApprovalDiffKind::Same, "[network.rules.api]"),
        (ApprovalDiffKind::Remove, "allow = [\"old.example.com\"]"),
        (ApprovalDiffKind::Add, "allow = [\"api.example.com\"]"),
    ]
    .into_iter()
    .map(|(kind, text)| ApprovalDiffLine {
        kind,
        text: text.into(),
    })
    .collect();
    let command = format!("curl {} WIDE-END", "-H 'x-long: header' ".repeat(8));
    let toml: Vec<String> = (0..30)
        .map(|i| format!("key{i} = {i}"))
        .chain(["LAST-LINE".into()])
        .collect();
    vec![
        ApprovalDetail {
            label: "Change".into(),
            value: String::new(),
            mono: true,
            diff: Some(diff),
        },
        detail("Command", &command, true),
        detail("Config", &toml.join("\n"), true),
    ]
}

/// Test that the dialog shows on top of the Monitor tab and that the
/// accept key allows the request. The shortcut letters work in both cases.
///   1. Open the Monitor tab and send a request
///   2. Check that the dialog and the tab show
///   3. Press `A` and check that the host gets `true` and the dialog closes
#[test]
fn accept_key_on_dialog_over_monitor_tab_allows_request() {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    let mut rx = ask(&mut tui, "Allow network access?", Vec::new());

    let screen = tui.screen();
    for text in ["Allow network access?", BODY, "accept", "deny", "show more"] {
        assert!(screen.contains(text), "{text:?} not on screen:\n{screen}");
    }
    assert!(screen.contains("airlock sandbox monitor"), "{screen}");

    tui.key(KeyCode::Char('A'));
    assert_eq!(rx.try_recv(), Ok(true));
    assert!(!tui.screen().contains("Allow network access?"));
}

/// Test that a click on the deny button denies the request, and that the
/// release of that click does not go to the guest.
///   1. Turn on guest mouse mode and send a request
///   2. Press and release the left button on the deny button
///   3. Check that the host gets `false`, the dialog closes and the guest
///      got nothing
#[test]
fn deny_button_click_denies_request() {
    let mut tui = Tui::new();
    tui.output(b"\x1b[?1000h\x1b[?1006h");
    let mut rx = ask(&mut tui, "Allow network access?", Vec::new());

    let at = tui.find("deny").unwrap();
    tui.click(at);
    tui.mouse(MouseEventKind::Up(MouseButton::Left), at);

    assert_eq!(rx.try_recv(), Ok(false));
    assert!(!tui.screen().contains("deny"));
    assert!(tui.sent().is_empty());
}

/// Test that the dialog gets all input. Keys, pastes and mouse events must
/// not go to the guest or to the tabs while the user decides. Only the
/// release of a press that the guest got before the dialog opened goes to
/// the guest, else the guest sees a button that stays down.
///   1. Turn on guest mouse mode and press the left button in the guest
///   2. Send a request, release the button and check that the guest got
///      the release
///   3. Type, paste, press Enter, Ctrl+C, Ctrl+D, Esc and F2, click and scroll
///   4. Check that the guest got nothing, no signal went out, the tab did
///      not change and the request has no answer
///   5. Accept, type again and check that the keys go to the guest
#[test]
fn input_while_dialog_shows_does_not_reach_guest() {
    let mut tui = Tui::new();
    tui.output(b"\x1b[?1000h\x1b[?1006h");
    tui.click((4, 2));
    assert_eq!(tui.sent_bytes(), b"\x1b[<0;5;3M");
    let mut rx = ask(&mut tui, "Allow network access?", Vec::new());
    tui.mouse(MouseEventKind::Up(MouseButton::Left), (4, 2));
    assert_eq!(tui.sent_bytes(), b"\x1b[<0;5;3m");

    tui.type_keys("xyz");
    tui.paste("rm -rf /\n");
    tui.key(KeyCode::Enter);
    tui.key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    tui.key_with(KeyCode::Char('d'), KeyModifiers::CONTROL);
    tui.key(KeyCode::Esc);
    tui.key(KeyCode::F(2));
    tui.click((2, 2));
    tui.mouse(MouseEventKind::ScrollUp, (2, 2));

    assert!(tui.sent().is_empty());
    assert!(tui.signals().is_empty());
    assert_eq!(tui.app.active_tab, Tab::Sandbox);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

    tui.key(KeyCode::Char('a'));
    tui.type_keys("ok");
    assert_eq!(rx.try_recv(), Ok(true));
    assert_eq!(tui.sent_bytes(), b"ok");
}

/// Test that the expanded dialog shows all details, and that the user can
/// scroll to content that is outside the dialog in both directions. The
/// scroll and the buttons must stay correct after a resize. As in the
/// desktop notification, a click on the short form shows the details.
///   1. Send a rule change request to a small terminal and click the body
///   2. Check the labels, the diff marks and the scroll bars, and that the
///      body does not show
///   3. Click the title and check that the details stay
///   4. Press End and check that the last TOML line shows
///   5. Press Home, scroll down with the wheel and check the last line again
///   6. Press Home, scroll right with `l` and check that the end of the
///      command shows
///   7. Press Esc and check that the short form shows again
///   8. Click show more, press End, make the terminal taller and press Up
///   9. Check that Up moved the visible content, then click deny
#[test]
fn expanded_details_scroll_to_show_off_screen_content() {
    let mut tui = Tui::with(60, 20, crate::TuiSettings::default());
    let mut rx = ask(&mut tui, "Change network rules?", rule_change_details());

    tui.click_text("The sandbox asks");
    let screen = tui.screen();
    for text in [
        "show less",
        "Change",
        "Command",
        "    [network.rules.api]",
        "- allow = [\"old.example.com\"]",
        "+ allow = [\"api.example.com\"]",
        "▼",
        "►",
    ] {
        assert!(screen.contains(text), "{text:?} not on screen:\n{screen}");
    }
    assert!(!screen.contains("LAST-LINE"), "{screen}");
    assert!(!screen.contains("WIDE-END"), "{screen}");
    assert!(!screen.contains("The sandbox asks"), "{screen}");

    tui.click_text("Change network rules?");
    assert!(tui.screen().contains("show less"));

    tui.key(KeyCode::End);
    assert!(tui.screen().contains("LAST-LINE"));

    tui.key(KeyCode::Home);
    assert!(!tui.screen().contains("LAST-LINE"));
    for _ in 0..20 {
        tui.mouse(MouseEventKind::ScrollDown, (30, 10));
    }
    assert!(tui.screen().contains("LAST-LINE"));

    tui.key(KeyCode::Home);
    for _ in 0..40 {
        tui.key(KeyCode::Char('l'));
    }
    assert!(tui.screen().contains("WIDE-END"));

    tui.key(KeyCode::Esc);
    let screen = tui.screen();
    assert!(screen.contains("show more"), "{screen}");
    assert!(!screen.contains("Command"), "{screen}");

    // After the resize, the old offset is past the new limit. One step up
    // from the new limit hides the last line.
    tui.click_text("show more");
    tui.key(KeyCode::End);
    tui.resize(60, 30);
    assert!(tui.screen().contains("LAST-LINE"));
    tui.key(KeyCode::Up);
    assert!(!tui.screen().contains("LAST-LINE"));

    tui.click_text("deny");
    assert_eq!(rx.try_recv(), Ok(false));
}

/// Test that requests show one at a time in the order they came, and that
/// requests that the host stopped to wait for go away.
///   1. Send requests A, B and C, and check that only A shows
///   2. Click accept on A and check that B shows
///   3. Stop the wait for C, deny B and check that no dialog shows
#[test]
fn queued_requests_show_one_at_a_time_in_order() {
    let mut tui = Tui::new();
    let mut rx_a = ask(&mut tui, "Request A", Vec::new());
    let mut rx_b = ask(&mut tui, "Request B", Vec::new());
    let rx_c = ask(&mut tui, "Request C", Vec::new());

    let screen = tui.screen();
    assert!(screen.contains("Request A"), "{screen}");
    assert!(!screen.contains("Request B"), "{screen}");

    tui.click_text("accept");
    assert_eq!(rx_a.try_recv(), Ok(true));
    assert!(tui.screen().contains("Request B"));

    drop(rx_c);
    tui.key(KeyCode::Char('d'));
    assert_eq!(rx_b.try_recv(), Ok(false));
    let screen = tui.screen();
    assert!(!screen.contains("Request C"), "{screen}");
    assert!(!screen.contains("accept"), "{screen}");
}

/// Test that the dialog goes away when the host stops the wait, for example
/// after a timeout. The next request shows in the short form.
///   1. Send requests A and B, and expand A
///   2. Drop the receiver of A
///   3. Check that B shows in the short form and that A got no answer
#[test]
fn closed_reply_removes_dialog_and_shows_next_in_short_form() {
    let mut tui = Tui::new();
    let rx_a = ask(
        &mut tui,
        "Request A",
        vec![detail("Host", "a.example.com", false)],
    );
    let _rx_b = ask(
        &mut tui,
        "Request B",
        vec![detail("Host", "b.example.com", false)],
    );
    tui.key(KeyCode::Char('s'));
    assert!(tui.screen().contains("a.example.com"));

    drop(rx_a);

    let screen = tui.screen();
    assert!(!screen.contains("Request A"), "{screen}");
    assert!(screen.contains("Request B"), "{screen}");
    assert!(screen.contains("show more"), "{screen}");
    assert!(!screen.contains("b.example.com"), "{screen}");
}

/// Test that a held key and a key right after a new dialog shows do not
/// answer. The user can aim the key at the guest or at the previous
/// dialog. A held key sends repeats, or fast presses without the kitty
/// keyboard protocol. The first repeat comes 250 to 660 ms after the
/// press. Thus a request that shows right after an answer ignores answers
/// for one second.
///   1. Use the real timing and send requests A and B
///   2. Press `a` at once and check that A has no answer
///   3. Wait, send a repeat of `a` and check that A has no answer
///   4. Press `x` and then `a` fast and check that A has no answer
///   5. Wait 200 ms, press `a` and check that A is allowed
///   6. Wait 500 ms, press `a`, click accept and check that B has no answer
///   7. Wait 600 ms more, press `a` and check that B is allowed
#[test]
fn held_or_early_answer_key_does_not_answer() {
    let mut tui = Tui::new();
    let real = crate::approval::Approvals::default();
    tui.app.approvals.grace = real.grace;
    tui.app.approvals.answer_grace = real.answer_grace;
    tui.app.approvals.key_gap = real.key_gap;
    let mut rx_a = ask(&mut tui, "Request A", Vec::new());
    let mut rx_b = ask(&mut tui, "Request B", Vec::new());

    tui.key(KeyCode::Char('a'));
    assert_eq!(rx_a.try_recv(), Err(TryRecvError::Empty));

    tui.app.approvals.age(Duration::from_secs(2));
    tui.send(TuiEvent::Terminal(Event::Key(
        KeyEvent::new_with_kind_and_state(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
            KeyEventState::NONE,
        ),
    )));
    assert_eq!(rx_a.try_recv(), Err(TryRecvError::Empty));

    tui.key(KeyCode::Char('x'));
    tui.key(KeyCode::Char('a'));
    assert_eq!(rx_a.try_recv(), Err(TryRecvError::Empty));

    tui.app.approvals.age(Duration::from_millis(200));
    tui.key(KeyCode::Char('a'));
    assert_eq!(rx_a.try_recv(), Ok(true));

    tui.app.approvals.age(Duration::from_millis(500));
    tui.key(KeyCode::Char('a'));
    tui.click_text("accept");
    assert_eq!(rx_b.try_recv(), Err(TryRecvError::Empty));
    assert!(tui.screen().contains("Request B"));

    tui.app.approvals.age(Duration::from_millis(600));
    tui.key(KeyCode::Char('a'));
    assert_eq!(rx_b.try_recv(), Ok(true));
}
