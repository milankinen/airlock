//! What `airlock start` does about packs: compare the configured packs with
//! the records in `installs.json`. Pure: the caller applies the result
//! ([`super::state::apply`]) and asks the questions.
//!
//! A record's fingerprint is the definition of its pack (name, version and
//! args, see [`crate::packs::ConfiguredPack::setup_installer`]). A pack
//! on the disk whose definition changed is "changed": only a new sandbox
//! installs it.

use super::state::{InstallState, PackStatus};

/// A configured pack (present and not `enabled = false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub id: String,
    /// [`crate::packs::InstallerScript::fingerprint`].
    pub fingerprint: String,
}

/// What [`decide`] compares.
pub struct DecideInput<'a> {
    pub state: &'a InstallState,
    /// The configured packs, in registry order.
    pub wanted: &'a [Wanted],
    /// [`crate::project::disk_id`] now.
    pub disk: Option<(u64, u64)>,
    /// The prepared image; `None` before the image is known (the early
    /// check), which then compares the disk only.
    pub image_id: Option<&'a str>,
}

/// Why a pack installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Not on the disk (as far as the records know).
    New,
    /// An install of the same fingerprint that did not finish (`unconfirmed`
    /// or `failed`), and no session ran on the disk since
    /// ([`InstallState::ran_session`]): it was approved for this disk
    /// already, so it installs again without a question. After a session
    /// it is `New`: the disk may hold code that session left.
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: String,
    pub why: Why,
}

/// A change to the records that needs no install run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The disk or image differs from the records: drop them all.
    Reset,
    /// `kept` (confirmed, same fingerprint) and configured again.
    Promote(String),
    /// A `failed` record of a pack that is no longer configured.
    Drop(String),
    /// The user kept a removed pack on the disk.
    Keep(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Packs to install, in registry order.
    pub pending: Vec<Pending>,
    /// Configured packs on the disk (installed, unconfirmed or kept) whose
    /// definition changed, in registry order: they install only into a new
    /// sandbox.
    pub changed: Vec<String>,
    /// Installed (or unconfirmed) packs that are no longer configured.
    pub removed: Vec<String>,
    pub transitions: Vec<Transition>,
}

/// Decide per pack ("configured" = in `wanted`):
///
/// | Record | Configured? | Result |
/// |---|---|---|
/// | any | no disk, or another disk | configured → `New`; records reset |
/// | not `failed`, other fingerprint | yes; same disk, other image | changed; records reset |
/// | other | yes; same disk, other image | `New`; records reset |
/// | none | yes | `New` |
/// | `installed`, same fingerprint | yes | nothing |
/// | `installed`, other fingerprint | yes | changed |
/// | `unconfirmed` or `failed`, same fingerprint | yes; no session since the install | `Retry` |
/// | `unconfirmed` or `failed`, same fingerprint | yes; a session since the install | `New` |
/// | `unconfirmed`, other fingerprint | yes | changed |
/// | `failed`, other fingerprint | yes | `New` |
/// | `kept`, confirmed, same fingerprint | yes | → `installed`, no run |
/// | `kept`, unconfirmed, same fingerprint | yes | `New` |
/// | `kept`, other fingerprint | yes | changed |
/// | `installed` or `unconfirmed` | no | removed (the caller asks) |
/// | `failed` | no | record dropped |
/// | `kept` | no | unchanged |
pub fn decide(input: &DecideInput<'_>) -> Plan {
    let state = input.state;
    let mut plan = Plan::default();
    let same_disk = input.disk.is_some() && state.disk == input.disk;
    let stale = !same_disk
        || input
            .image_id
            .is_some_and(|image| state.image_id.as_deref() != Some(image));
    if stale {
        if !state.packs.is_empty() {
            plan.transitions.push(Transition::Reset);
        }
        for w in input.wanted {
            // The disk stays with a new image (its old image is gone): it
            // still has the old definition of a changed pack.
            let changed = same_disk
                && state.packs.get(&w.id).is_some_and(|r| {
                    r.fingerprint != w.fingerprint
                        && matches!(
                            r.status,
                            PackStatus::Installed
                                | PackStatus::Unconfirmed
                                | PackStatus::Kept { .. }
                        )
                });
            if changed {
                plan.changed.push(w.id.clone());
            } else {
                plan.pending.push(Pending {
                    id: w.id.clone(),
                    why: Why::New,
                });
            }
        }
        return plan;
    }

    for w in input.wanted {
        let why = match state.packs.get(&w.id) {
            None => Some(Why::New),
            Some(r) => {
                let same = r.fingerprint == w.fingerprint;
                match r.status {
                    PackStatus::Installed if same => None,
                    PackStatus::Kept { confirmed: true } if same => {
                        plan.transitions.push(Transition::Promote(w.id.clone()));
                        None
                    }
                    PackStatus::Unconfirmed | PackStatus::Failed if same && state.ran_session => {
                        Some(Why::New)
                    }
                    PackStatus::Unconfirmed | PackStatus::Failed if same => Some(Why::Retry),
                    PackStatus::Kept { confirmed: false } if same => Some(Why::New),
                    PackStatus::Failed => Some(Why::New),
                    PackStatus::Installed | PackStatus::Unconfirmed | PackStatus::Kept { .. } => {
                        plan.changed.push(w.id.clone());
                        None
                    }
                }
            }
        };
        if let Some(why) = why {
            plan.pending.push(Pending {
                id: w.id.clone(),
                why,
            });
        }
    }

    for (id, r) in &state.packs {
        if input.wanted.iter().any(|w| &w.id == id) {
            continue;
        }
        match r.status {
            PackStatus::Installed | PackStatus::Unconfirmed => plan.removed.push(id.clone()),
            PackStatus::Failed => plan.transitions.push(Transition::Drop(id.clone())),
            PackStatus::Kept { .. } => {}
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::packs::install::state::Record;

    const DISK: Option<(u64, u64)> = Some((1, 2));
    const IMAGE: &str = "sha256:1";

    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn state(records: &[(&str, PackStatus, char)]) -> InstallState {
        let packs: BTreeMap<String, Record> = records
            .iter()
            .map(|(id, status, c)| {
                (
                    (*id).to_string(),
                    Record {
                        status: *status,
                        fingerprint: fp(*c),
                        at: 1,
                    },
                )
            })
            .collect();
        InstallState {
            disk: DISK,
            image_id: Some(IMAGE.into()),
            packs,
            ..InstallState::default()
        }
    }

    fn wanted(packs: &[(&str, char)]) -> Vec<Wanted> {
        packs
            .iter()
            .map(|(id, c)| Wanted {
                id: (*id).to_string(),
                fingerprint: fp(*c),
            })
            .collect()
    }

    fn run(state: &InstallState, wanted: &[Wanted]) -> Plan {
        decide(&DecideInput {
            state,
            wanted,
            disk: DISK,
            image_id: Some(IMAGE),
        })
    }

    fn pending(id: &str, why: Why) -> Pending {
        Pending { id: id.into(), why }
    }

    #[test]
    fn other_disk_image_or_no_disk_reinstalls_everything() {
        let s = state(&[
            ("a", PackStatus::Installed, 'a'),
            ("gone", PackStatus::Kept { confirmed: true }, 'a'),
        ]);
        let w = wanted(&[("a", 'a'), ("b", 'b')]);
        let expected = Plan {
            pending: vec![pending("a", Why::New), pending("b", Why::New)],
            changed: vec![],
            removed: vec![],
            transitions: vec![Transition::Reset],
        };
        for (disk, image) in [
            (Some((9, 9)), Some(IMAGE)),
            (None, Some(IMAGE)),
            (DISK, Some("sha256:2")),
            (None, None),
        ] {
            let plan = decide(&DecideInput {
                state: &s,
                wanted: &w,
                disk,
                image_id: image,
            });
            assert_eq!(plan, expected, "{disk:?} {image:?}");
        }
        // Without the image (the early check), only the disk counts.
        let plan = decide(&DecideInput {
            state: &s,
            wanted: &w,
            disk: DISK,
            image_id: None,
        });
        assert_eq!(plan.pending, vec![pending("b", Why::New)]);
        // No records: nothing to reset.
        let plan = decide(&DecideInput {
            state: &InstallState::default(),
            wanted: &w,
            disk: None,
            image_id: None,
        });
        assert!(plan.transitions.is_empty());
        assert_eq!(plan.pending.len(), 2);
    }

    #[test]
    fn no_record_is_new() {
        let plan = run(&state(&[]), &wanted(&[("a", 'a')]));
        assert_eq!(plan.pending, vec![pending("a", Why::New)]);
    }

    #[test]
    fn installed_same_fingerprint_is_nothing() {
        let s = state(&[("a", PackStatus::Installed, 'a')]);
        assert_eq!(run(&s, &wanted(&[("a", 'a')])), Plan::default());
    }

    #[test]
    fn installed_other_fingerprint_is_changed() {
        let s = state(&[("a", PackStatus::Installed, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'b')]));
        assert!(plan.pending.is_empty());
        assert_eq!(plan.changed, ["a"]);
    }

    #[test]
    fn unconfirmed_is_retry_with_the_same_fingerprint_else_changed() {
        let s = state(&[("a", PackStatus::Unconfirmed, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'a')]));
        assert_eq!(plan.pending, vec![pending("a", Why::Retry)]);
        let plan = run(&s, &wanted(&[("a", 'b')]));
        assert!(plan.pending.is_empty());
        assert_eq!(plan.changed, ["a"]);
    }

    #[test]
    fn failed_is_retry_with_the_same_fingerprint_else_new() {
        let s = state(&[("a", PackStatus::Failed, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'a')]));
        assert_eq!(plan.pending, vec![pending("a", Why::Retry)]);
        let plan = run(&s, &wanted(&[("a", 'b')]));
        assert_eq!(plan.pending, vec![pending("a", Why::New)]);
    }

    /// A session since the unfinished install may have left code on the
    /// disk: the install is `New` (asked), not a silent `Retry`.
    #[test]
    fn a_session_since_the_install_makes_a_retry_new() {
        for status in [PackStatus::Unconfirmed, PackStatus::Failed] {
            let s = InstallState {
                ran_session: true,
                ..state(&[("a", status, 'a')])
            };
            let plan = run(&s, &wanted(&[("a", 'a')]));
            assert_eq!(plan.pending, vec![pending("a", Why::New)], "{status:?}");
        }
    }

    #[test]
    fn kept_confirmed_same_fingerprint_is_promoted() {
        let s = state(&[("a", PackStatus::Kept { confirmed: true }, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'a')]));
        assert_eq!(
            plan,
            Plan {
                transitions: vec![Transition::Promote("a".into())],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn kept_unconfirmed_is_new_and_other_fingerprint_is_changed() {
        let s = state(&[("a", PackStatus::Kept { confirmed: false }, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'a')]));
        assert_eq!(plan.pending, vec![pending("a", Why::New)]);
        assert!(plan.transitions.is_empty());

        let s = state(&[("a", PackStatus::Kept { confirmed: true }, 'a')]);
        let plan = run(&s, &wanted(&[("a", 'b')]));
        assert!(plan.pending.is_empty());
        assert_eq!(plan.changed, ["a"]);
        assert!(plan.transitions.is_empty());
    }

    #[test]
    fn installed_or_unconfirmed_unconfigured_is_removed() {
        let s = state(&[
            ("a", PackStatus::Installed, 'a'),
            ("b", PackStatus::Unconfirmed, 'a'),
        ]);
        let plan = run(&s, &[]);
        assert_eq!(plan.removed, ["a", "b"]);
        assert!(plan.pending.is_empty() && plan.transitions.is_empty());
    }

    #[test]
    fn failed_unconfigured_is_dropped() {
        let s = state(&[("a", PackStatus::Failed, 'a')]);
        let plan = run(&s, &[]);
        assert_eq!(plan.transitions, [Transition::Drop("a".into())]);
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn kept_unconfigured_is_unchanged() {
        for confirmed in [true, false] {
            let s = state(&[("a", PackStatus::Kept { confirmed }, 'a')]);
            assert_eq!(run(&s, &[]), Plan::default());
        }
    }
}
