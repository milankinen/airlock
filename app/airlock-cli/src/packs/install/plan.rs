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

    fn state(records: &[(&str, PackStatus, char)], ran_session: bool) -> InstallState {
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
            ran_session,
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

    fn decide_on(
        state: &InstallState,
        wanted: &[Wanted],
        disk: Option<(u64, u64)>,
        image_id: Option<&str>,
    ) -> Plan {
        decide(&DecideInput {
            state,
            wanted,
            disk,
            image_id,
        })
    }

    fn new(ids: &[&str]) -> Vec<Pending> {
        ids.iter()
            .map(|id| Pending {
                id: (*id).to_string(),
                why: Why::New,
            })
            .collect()
    }

    #[test]
    fn other_disk_or_image_resets_records_and_installs_every_configured_pack() {
        let s = state(
            &[
                ("a", PackStatus::Installed, 'a'),
                ("gone", PackStatus::Kept { confirmed: true }, 'a'),
            ],
            false,
        );
        let w = wanted(&[("a", 'a'), ("b", 'b')]);
        let reinstall = Plan {
            pending: new(&["a", "b"]),
            transitions: vec![Transition::Reset],
            ..Plan::default()
        };
        for (disk, image) in [
            (Some((9, 9)), Some(IMAGE)),
            (None, Some(IMAGE)),
            (DISK, Some("sha256:2")),
            (None, None),
        ] {
            assert_eq!(
                decide_on(&s, &w, disk, image),
                reinstall,
                "{disk:?} {image:?}"
            );
        }
        assert_eq!(decide_on(&s, &w, DISK, None).pending, new(&["b"]));

        let empty = decide_on(&InstallState::default(), &w, None, None);
        assert_eq!(empty.pending, new(&["a", "b"]));
        assert!(empty.transitions.is_empty());

        let s = state(
            &[
                ("a", PackStatus::Installed, 'a'),
                ("b", PackStatus::Failed, 'a'),
            ],
            false,
        );
        let plan = decide_on(
            &s,
            &wanted(&[("a", 'b'), ("b", 'b')]),
            DISK,
            Some("sha256:2"),
        );
        assert_eq!(plan.changed, ["a"]);
        assert_eq!(plan.pending, new(&["b"]));
        assert_eq!(plan.transitions, [Transition::Reset]);
    }

    #[test]
    fn configured_pack_installs_retries_or_changes_by_its_record() {
        let retry = Some(Why::Retry);
        let fresh = Some(Why::New);
        let unconfirmed_kept = PackStatus::Kept { confirmed: false };
        let confirmed_kept = PackStatus::Kept { confirmed: true };
        let rows = [
            (None, 'a', false, fresh, false),
            (Some(PackStatus::Installed), 'a', true, None, false),
            (Some(PackStatus::Installed), 'b', false, None, true),
            (Some(PackStatus::Unconfirmed), 'a', false, retry, false),
            (Some(PackStatus::Unconfirmed), 'a', true, fresh, false),
            (Some(PackStatus::Unconfirmed), 'b', false, None, true),
            (Some(PackStatus::Failed), 'a', false, retry, false),
            (Some(PackStatus::Failed), 'a', true, fresh, false),
            (Some(PackStatus::Failed), 'b', false, fresh, false),
            (Some(confirmed_kept), 'a', true, None, false),
            (Some(unconfirmed_kept), 'a', false, fresh, false),
            (Some(confirmed_kept), 'b', false, None, true),
            (Some(unconfirmed_kept), 'b', false, None, true),
            (Some(PackStatus::Installed), 'a', false, None, false),
        ];
        for (status, wanted_fp, ran_session, why, changed) in rows {
            let records: Vec<(&str, PackStatus, char)> =
                status.map(|s| ("a", s, 'a')).into_iter().collect();
            let plan = decide_on(
                &state(&records, ran_session),
                &wanted(&[("a", wanted_fp)]),
                DISK,
                Some(IMAGE),
            );
            let row = format!("{status:?} {wanted_fp} session={ran_session}");
            let pending: Vec<(String, Why)> =
                why.map(|why| ("a".to_string(), why)).into_iter().collect();
            assert_eq!(
                plan.pending
                    .iter()
                    .map(|p| (p.id.clone(), p.why))
                    .collect::<Vec<_>>(),
                pending,
                "{row}"
            );
            assert_eq!(!plan.changed.is_empty(), changed, "{row}");
            assert!(plan.removed.is_empty(), "{row}");
            let promoted = status == Some(confirmed_kept) && wanted_fp == 'a';
            let expected = if promoted {
                vec![Transition::Promote("a".into())]
            } else {
                vec![]
            };
            assert_eq!(plan.transitions, expected, "{row}");
        }
    }

    #[test]
    fn unconfigured_pack_is_removed_dropped_or_kept_by_its_record() {
        let s = state(
            &[
                ("a", PackStatus::Installed, 'a'),
                ("b", PackStatus::Unconfirmed, 'a'),
                ("c", PackStatus::Failed, 'a'),
                ("d", PackStatus::Kept { confirmed: true }, 'a'),
                ("e", PackStatus::Kept { confirmed: false }, 'a'),
            ],
            true,
        );
        let plan = decide_on(&s, &[], DISK, Some(IMAGE));
        assert_eq!(
            plan,
            Plan {
                removed: vec!["a".into(), "b".into()],
                transitions: vec![Transition::Drop("c".into())],
                ..Plan::default()
            }
        );
    }
}
