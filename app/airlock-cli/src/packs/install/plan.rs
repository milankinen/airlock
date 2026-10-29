//! Pack install planning.
//!
//! Compares the configured packs with the install state and decides what
//! `airlock start` must install, and which install records change. The
//! planning has no side effects. The caller applies the result and asks
//! the user about changed, removed and added packs.

use super::state::{InstallState, PackStatus};

/// A configured pack (present and not `enabled = false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// Pack name.
    pub id: String,
    /// Pack definition (name, version and args), see
    /// [`crate::packs::InstallerScript::fingerprint`].
    pub fingerprint: String,
}

/// Input of [`decide`].
pub struct DecideInput<'a> {
    /// Install records of the sandbox disk.
    pub state: &'a InstallState,
    /// The configured packs, in pack order (by kind, then by name).
    pub wanted: &'a [Wanted],
    /// Current [`crate::project::disk_id`].
    pub disk: Option<(u64, u64)>,
    /// ID of the prepared image. `None` before the image is known (the
    /// early check). Then [`decide`] compares only the disk.
    pub image_id: Option<&'a str>,
}

/// Reason for a pack install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The pack is not on the disk (as far as the records know).
    New,
    /// An install of the same fingerprint did not finish (`unconfirmed` or
    /// `failed`), and no session ran on the disk after it (see
    /// [`InstallState::ran_session`]). The user approved it for this disk
    /// before, so it installs again without a question. After a session it
    /// is `New`, because the disk may contain code that the session left.
    Retry,
    /// The disk has an older definition of the pack, and the user chose to
    /// install the new definition on top of it (see
    /// [`Plan::reinstall_changed`]).
    Changed,
}

/// A pack to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Pack name.
    pub id: String,
    /// Reason for the install.
    pub why: Why,
}

/// A change to the records that needs no install run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The disk or image is not the same as in the records. Remove all
    /// records.
    Reset,
    /// The pack is `kept` (confirmed, same fingerprint) and configured
    /// again. Mark it `installed`.
    Promote(String),
    /// A `failed` record of a pack that is no longer configured.
    Drop(String),
    /// The user kept a removed pack on the disk.
    Keep(String),
    /// The user kept the old definition of a changed pack on the disk.
    /// Record the new fingerprint and keep the status. Thus the pack does
    /// not count as changed again.
    Accept {
        /// Pack id.
        id: String,
        /// The new fingerprint.
        fingerprint: String,
    },
}

/// Result of [`decide`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Packs to install, in the order of `wanted`.
    pub pending: Vec<Pending>,
    /// Configured packs on the disk (installed, unconfirmed or kept) with a
    /// changed definition, in the order of `wanted`. The caller asks the
    /// user: re-create the sandbox, [`Plan::reinstall_changed`] or
    /// [`Plan::accept_changed`].
    pub changed: Vec<String>,
    /// Installed (or unconfirmed) packs that are no longer configured.
    pub removed: Vec<String>,
    /// Record changes that need no install run.
    pub transitions: Vec<Transition>,
}

impl Plan {
    /// Install the changed packs again, on top of the disk (see
    /// [`Why::Changed`]).
    /// Args:
    ///  - `wanted`: The configured packs, as given to [`decide`]. The
    ///    pending packs stay in this order.
    pub fn reinstall_changed(&mut self, wanted: &[Wanted]) {
        let changed = std::mem::take(&mut self.changed);
        self.pending.extend(changed.into_iter().map(|id| Pending {
            id,
            why: Why::Changed,
        }));
        self.pending
            .sort_by_key(|p| wanted.iter().position(|w| w.id == p.id));
    }

    /// Keep the old definitions of the changed packs on the disk (see
    /// [`Transition::Accept`]).
    /// Args:
    ///  - `wanted`: The configured packs, as given to [`decide`]
    pub fn accept_changed(&mut self, wanted: &[Wanted]) {
        for id in std::mem::take(&mut self.changed) {
            if let Some(w) = wanted.iter().find(|w| w.id == id) {
                self.transitions.push(Transition::Accept {
                    id,
                    fingerprint: w.fingerprint.clone(),
                });
            }
        }
    }
}

/// Compare the configured packs with the install records.
///
/// A record fingerprint identifies the definition of its pack (name,
/// version and args, see [`crate::packs::ConfiguredPack::setup_installer`]).
/// A pack on the disk with a changed definition is "changed". The caller
/// asks the user about it.
///
/// Decision for each pack ("configured" means in `wanted`):
///
/// | Record | Configured? | Result |
/// |---|---|---|
/// | any | no disk, another disk, or another image | configured → `New`, records reset |
/// | none | yes | `New` |
/// | `installed`, same fingerprint | yes | nothing |
/// | `installed`, other fingerprint | yes | changed |
/// | `unconfirmed` or `failed`, same fingerprint | yes (no session since the install) | `Retry` |
/// | `unconfirmed` or `failed`, same fingerprint | yes (a session since the install) | `New` |
/// | `unconfirmed`, other fingerprint | yes | changed |
/// | `failed`, other fingerprint | yes | `New` |
/// | `kept`, confirmed, same fingerprint | yes | → `installed`, no run |
/// | `kept`, unconfirmed, same fingerprint | yes | `New` |
/// | `kept`, other fingerprint | yes | changed |
/// | `installed` or `unconfirmed` | no | removed (the caller asks) |
/// | `failed` | no | record dropped |
/// | `kept` | no | unchanged |
///
/// Returns:
///   The plan. [`super::state::apply`] applies it.
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
        // The records do not describe the disk any more, so all packs
        // install as new. A disk that stays with a new image (its old image
        // is gone) gets the added-tools question.
        plan.pending.extend(input.wanted.iter().map(|w| Pending {
            id: w.id.clone(),
            why: Why::New,
        }));
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
    //! Tests of the install plan: what to install and which records change.

    use std::collections::BTreeMap;

    use super::*;
    use crate::packs::install::state::Record;

    /// The disk and image of the records in [`state`].
    const DISK: Option<(u64, u64)> = Some((1, 2));
    const IMAGE: &str = "sha256:1";

    /// A fingerprint of 64 copies of `c`.
    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// A state on [`DISK`] and [`IMAGE`] with `records` of
    /// `(id, status, fingerprint char)`.
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

    /// Configured packs of `(id, fingerprint char)`.
    fn wanted(packs: &[(&str, char)]) -> Vec<Wanted> {
        packs
            .iter()
            .map(|(id, c)| Wanted {
                id: (*id).to_string(),
                fingerprint: fp(*c),
            })
            .collect()
    }

    /// The plan for `state` and `wanted` on the disk and image.
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

    /// Pending packs that are all new.
    fn new(ids: &[&str]) -> Vec<Pending> {
        ids.iter()
            .map(|id| Pending {
                id: (*id).to_string(),
                why: Why::New,
            })
            .collect()
    }

    /// Test that another disk or image resets the records and installs all
    /// configured packs. The records describe only the disk and image that
    /// they were made on.
    ///   1. Check that another disk, no disk or another image gives a reset
    ///      and installs all packs as new
    ///   2. Check that without a known image only the disk counts
    ///   3. Check that an empty state installs all packs with no reset
    ///   4. On the same disk with another image, check that a pack with a
    ///      changed definition and a failed pack are both new
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
        assert!(plan.changed.is_empty());
        assert_eq!(plan.pending, new(&["a", "b"]));
        assert_eq!(plan.transitions, [Transition::Reset]);
    }

    /// Test the two answers to the changed-packs question that keep the
    /// disk. A reinstall installs the changed packs on top of the disk in
    /// pack order. "Keep old" records the new fingerprints and keeps the
    /// status, so the packs do not count as changed again.
    ///   1. Plan an installed, an unconfirmed and a kept pack with changed
    ///      definitions, and a new pack between them
    ///   2. Reinstall and check the pending packs and their reasons
    ///   3. Plan again, accept, and apply the transitions to the records
    ///   4. Check that the records have the new fingerprints and the old
    ///      status, and that a new plan has no changed packs
    #[test]
    fn changed_packs_reinstall_on_top_or_keep_old_definition() {
        let s = state(
            &[
                ("a", PackStatus::Installed, 'a'),
                ("c", PackStatus::Unconfirmed, 'a'),
                ("d", PackStatus::Kept { confirmed: true }, 'a'),
            ],
            false,
        );
        let w = wanted(&[("a", 'b'), ("b", 'b'), ("c", 'b'), ("d", 'b')]);
        let mut plan = decide_on(&s, &w, DISK, Some(IMAGE));
        assert_eq!(plan.changed, ["a", "c", "d"]);
        plan.reinstall_changed(&w);
        assert!(plan.changed.is_empty());
        let pending: Vec<(&str, Why)> = plan
            .pending
            .iter()
            .map(|p| (p.id.as_str(), p.why))
            .collect();
        assert_eq!(
            pending,
            [
                ("a", Why::Changed),
                ("b", Why::New),
                ("c", Why::Changed),
                ("d", Why::Changed)
            ]
        );

        let mut plan = decide_on(&s, &w, DISK, Some(IMAGE));
        plan.accept_changed(&w);
        assert!(plan.changed.is_empty());
        assert_eq!(plan.pending, new(&["b"]));
        let mut accepted = s.clone();
        crate::packs::install::state::apply(&mut accepted, &plan.transitions);
        for (id, status) in [
            ("a", PackStatus::Installed),
            ("c", PackStatus::Unconfirmed),
            ("d", PackStatus::Kept { confirmed: true }),
        ] {
            assert_eq!(accepted.packs[id].fingerprint, fp('b'), "{id}");
            assert_eq!(accepted.packs[id].status, status, "{id}");
        }
        let again = decide_on(&accepted, &w, DISK, Some(IMAGE));
        assert!(again.changed.is_empty());
        assert_eq!(again.transitions, [Transition::Promote("d".into())]);
    }

    /// Test the decision for one configured pack for each kind of record,
    /// fingerprint and session flag. This is the table in the doc of
    /// `decide`.
    ///   1. For each row, make a state with the record and plan the pack
    ///   2. Check the pending reason and if the pack is changed
    ///   3. Check that only a confirmed kept record with the same
    ///      fingerprint gives a promote transition
    #[test]
    fn configured_pack_installs_retries_or_changes_by_its_record() {
        let retry = Some(Why::Retry);
        let fresh = Some(Why::New);
        let unconfirmed_kept = PackStatus::Kept { confirmed: false };
        let confirmed_kept = PackStatus::Kept { confirmed: true };
        // (record status, wanted fingerprint, session ran, pending reason,
        // changed). The record fingerprint is always 'a'.
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

    /// Test the decision for packs that are no longer configured. Installed
    /// and unconfirmed packs are removed, failed records are dropped and
    /// kept records stay.
    ///   1. Make a state with one record of each status
    ///   2. Plan with no configured packs and check the plan
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
