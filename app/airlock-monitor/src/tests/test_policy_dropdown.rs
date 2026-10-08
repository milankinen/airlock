use crossterm::event::KeyCode;

use crate::test_cfg::Tui;
use crate::{NetworkControl, Policy};

fn monitor() -> Tui {
    let mut tui = Tui::new();
    tui.key(KeyCode::F(2));
    tui
}

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
    assert!(!tui.screen().contains("Always allow"));
}

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
