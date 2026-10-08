//! Network policy dropdown on the Monitor tab, with keys and with the mouse.

use crossterm::event::KeyCode;

use crate::test_cfg::Tui;
use crate::{NetworkControl, Policy};

/// Make a TUI with the Monitor tab open.
fn monitor() -> Tui {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    tui
}

/// Test that a policy selected with keys in the dropdown goes to the host.
///   1. Open the dropdown and check that it shows all policies
///   2. Move down one item and confirm
///   3. Check the host policy, the header text and that the dropdown closed
#[test]
fn policy_chosen_with_keys_is_applied_to_host() {
    let mut tui = monitor();
    assert!(tui.screen().contains("policy: Allow by default"));

    tui.key(KeyCode::Char('p'));
    let screen = tui.screen();
    for policy in Policy::ALL {
        assert!(screen.contains(policy.title()), "{screen}");
    }
    tui.key(KeyCode::Down);
    tui.key(KeyCode::Enter);

    assert_eq!(tui.network.policy(), Policy::DenyByDefault);
    assert!(tui.screen().contains("policy: Deny by default"));
    // "Always allow" shows only in the open dropdown.
    assert!(!tui.screen().contains("Always allow"));
}

/// Test that a cancelled policy dropdown does not change the policy.
///   1. Open the dropdown, move the highlight and press Esc
///   2. Check that the policy did not change and the dropdown closed
///   3. Open the dropdown and press the back key
///   4. Check that the Monitor tab stays open and the policy did not change
#[test]
fn policy_dropdown_cancelled_leaves_policy_unchanged() {
    let mut tui = monitor();
    tui.key(KeyCode::Char('p'));
    tui.key(KeyCode::Up);
    tui.key(KeyCode::Esc);

    assert_eq!(tui.network.policy(), Policy::AllowByDefault);
    assert!(!tui.screen().contains("Always allow"));

    tui.key(KeyCode::Char('p'));
    tui.key(KeyCode::Char('q'));
    assert!(tui.screen().contains("airlock sandbox monitor"));
    assert_eq!(tui.network.policy(), Policy::AllowByDefault);
}

/// Test that a policy clicked in the dropdown goes to the host, and that a
/// click outside the dropdown closes it without a change.
///   1. Click the policy header, then click "Always deny"
///   2. Check the host policy and the header text
///   3. Open the dropdown and click outside it
///   4. Check that the policy did not change and the dropdown closed
#[test]
fn policy_chosen_with_mouse_is_applied_to_host() {
    let mut tui = monitor();
    tui.click_text("policy:");
    tui.click_text("Always deny");

    assert_eq!(tui.network.policy(), Policy::DenyAlways);
    assert!(tui.screen().contains("policy: Always deny"));

    tui.click_text("policy:");
    tui.click((5, 25));
    assert_eq!(tui.network.policy(), Policy::DenyAlways);
    assert!(!tui.screen().contains("Always allow"));
}
