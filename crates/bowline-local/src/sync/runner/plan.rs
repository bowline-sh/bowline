//! Pure sync decision planner.
//!
//! `plan_sync_action` is the store-free, IO-free port of `tick`'s B1..B5 branch
//! cascade. Keeping it pure is the entire testability win: the decision can be
//! exercised by a branch-table unit test that builds three snapshot ids and two
//! small state enums instead of a full `SnapshotCandidate`/`ScanReport`.

use super::helpers::EMPTY_SNAPSHOT_ID;

/// The decision a sync tick reaches before any IO/CAS/persistence runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncAction {
    NoChanges,
    Import,
    Materialize,
    Upload,
    StaleMerge,
}

/// A snapshot id crossing the planner boundary. Newtype so the pure planner
/// never receives a raw domain `String`; conversion happens at the runner edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SnapshotId(String);

impl SnapshotId {
    pub(super) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The three snapshot ids the cascade compares, grouped so ids do not cross the
/// planner boundary as loose `String`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservedSnapshotIds {
    base: SnapshotId,
    candidate_base: SnapshotId,
    candidate: SnapshotId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalHeadState {
    Absent,
    MatchesBase,
    Diverged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateEntryState {
    Empty,
    Present,
}

impl ObservedSnapshotIds {
    pub(super) fn new(base: SnapshotId, candidate_base: SnapshotId, candidate: SnapshotId) -> Self {
        Self {
            base,
            candidate_base,
            candidate,
        }
    }
}

/// The minimal, cheap facts the pure planner consumes (KTD-1). `observe` builds
/// this alongside the full `SyncObservation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncDecisionFacts {
    snapshots: ObservedSnapshotIds,
    local_head: LocalHeadState,
    candidate_entries: CandidateEntryState,
}

impl SyncDecisionFacts {
    pub(super) fn new(
        snapshots: ObservedSnapshotIds,
        local_head: LocalHeadState,
        candidate_entries: CandidateEntryState,
    ) -> Self {
        Self {
            snapshots,
            local_head,
            candidate_entries,
        }
    }
}

pub(super) fn plan_sync_action(f: &SyncDecisionFacts) -> SyncAction {
    // Arms are ordered, not disjoint; earlier arms shadow later ones. B2 shadows
    // B4/B5. Do not reorder or merge conditions.
    let s = &f.snapshots;
    if s.candidate == s.base && f.local_head == LocalHeadState::MatchesBase {
        return SyncAction::NoChanges; // B1
    }
    if s.candidate == s.base {
        // Matching scan output is not authority to adopt a remote head the
        // device has never completed materializing. Stat metadata can survive
        // an interrupted import, so replay the import before advancing the
        // local head.
        return SyncAction::Import;
    }
    if s.candidate == s.candidate_base {
        // candidate != base here (B1 returned when equal), so candidate ==
        // candidate_base forces candidate_base != base: always Import (B2).
        return SyncAction::Import;
    }
    if f.local_head == LocalHeadState::Absent
        && s.base.as_str() != EMPTY_SNAPSHOT_ID
        && f.candidate_entries == CandidateEntryState::Empty
    {
        return SyncAction::Materialize; // B3
    }
    if s.candidate_base != s.base {
        return SyncAction::StaleMerge; // B4
    }
    SyncAction::Upload // B5
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FactsFixture<'a> {
        snapshots: [&'a str; 3],
        local_head: LocalHeadState,
        candidate_entries: CandidateEntryState,
    }

    fn facts(fixture: FactsFixture<'_>) -> SyncDecisionFacts {
        let [base, candidate_base, candidate] = fixture.snapshots;
        SyncDecisionFacts::new(
            ObservedSnapshotIds::new(
                SnapshotId::new(base),
                SnapshotId::new(candidate_base),
                SnapshotId::new(candidate),
            ),
            fixture.local_head,
            fixture.candidate_entries,
        )
    }

    struct Row {
        name: &'static str,
        facts: SyncDecisionFacts,
        expected: SyncAction,
    }

    #[test]
    fn plan_sync_action_covers_every_branch() {
        let rows = vec![
            // A matching candidate is NoChanges only after this device has
            // already committed the remote base as its local head.
            Row {
                name: "candidate==base, differing candidate_base, head absent",
                facts: facts(FactsFixture {
                    snapshots: ["base", "other", "base"],
                    local_head: LocalHeadState::Absent,
                    candidate_entries: CandidateEntryState::Empty,
                }),
                expected: SyncAction::Import,
            },
            Row {
                name: "candidate==base, head present",
                facts: facts(FactsFixture {
                    snapshots: ["base", "other", "base"],
                    local_head: LocalHeadState::MatchesBase,
                    candidate_entries: CandidateEntryState::Present,
                }),
                expected: SyncAction::NoChanges,
            },
            // B1 precedence: candidate == base == candidate_base with an
            // authoritative local head ⇒ NoChanges.
            Row {
                name: "candidate==base==candidate_base traces B1",
                facts: facts(FactsFixture {
                    snapshots: ["same", "same", "same"],
                    local_head: LocalHeadState::MatchesBase,
                    candidate_entries: CandidateEntryState::Present,
                }),
                expected: SyncAction::NoChanges,
            },
            // candidate == candidate_base, candidate_base != base ⇒ Import (B2).
            Row {
                name: "candidate==candidate_base != base ⇒ Import",
                facts: facts(FactsFixture {
                    snapshots: ["base", "cand", "cand"],
                    local_head: LocalHeadState::Diverged,
                    candidate_entries: CandidateEntryState::Present,
                }),
                expected: SyncAction::Import,
            },
            // local_head absent, base != "empty", candidate entries empty ⇒ Materialize (B3).
            Row {
                name: "head absent, base!=empty, entries empty ⇒ Materialize",
                facts: facts(FactsFixture {
                    snapshots: ["base", "base", "cand"],
                    local_head: LocalHeadState::Absent,
                    candidate_entries: CandidateEntryState::Empty,
                }),
                expected: SyncAction::Materialize,
            },
            // Boundary: head absent, base == "empty", entries empty ⇒ NOT Materialize;
            // candidate_base==base ⇒ Upload (proves the EMPTY_SNAPSHOT_ID guard is load-bearing).
            Row {
                name: "head absent, base==empty, entries empty ⇒ Upload",
                facts: facts(FactsFixture {
                    snapshots: [EMPTY_SNAPSHOT_ID, EMPTY_SNAPSHOT_ID, "cand"],
                    local_head: LocalHeadState::Absent,
                    candidate_entries: CandidateEntryState::Empty,
                }),
                expected: SyncAction::Upload,
            },
            // local_head present, entries empty, candidate_base==base ⇒ Upload (Materialize needs absent head).
            Row {
                name: "head present, entries empty, candidate_base==base ⇒ Upload",
                facts: facts(FactsFixture {
                    snapshots: ["base", "base", "cand"],
                    local_head: LocalHeadState::MatchesBase,
                    candidate_entries: CandidateEntryState::Empty,
                }),
                expected: SyncAction::Upload,
            },
            // candidate_base != base, candidate differs from both ⇒ StaleMerge (B4).
            Row {
                name: "candidate_base != base, candidate differs ⇒ StaleMerge",
                facts: facts(FactsFixture {
                    snapshots: ["base", "stalebase", "cand"],
                    local_head: LocalHeadState::Diverged,
                    candidate_entries: CandidateEntryState::Present,
                }),
                expected: SyncAction::StaleMerge,
            },
            // candidate_base == base, candidate differs ⇒ Upload (B5).
            Row {
                name: "candidate_base == base, candidate differs ⇒ Upload",
                facts: facts(FactsFixture {
                    snapshots: ["base", "base", "cand"],
                    local_head: LocalHeadState::MatchesBase,
                    candidate_entries: CandidateEntryState::Present,
                }),
                expected: SyncAction::Upload,
            },
        ];

        for row in rows {
            let action = plan_sync_action(&row.facts);
            assert_eq!(action, row.expected, "action for row: {}", row.name);
        }
    }
}
