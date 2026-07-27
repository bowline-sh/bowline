use crate::status_projection::{
    SourceFreshness, SourceRevision, StatusSource, StatusSourceFacts, StatusSourceRevision,
    StatusTimestamp, engine_convergence_facts, replace_convergence_status,
    scoped_engine_convergence_facts,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use bowline_core::commands::StatusCommandOutput;
use bowline_core::status::overlay_convergence_status;
use bowline_local::sync::manifest_engine::{
    Degradation, EnginePhase, EngineSnapshot, WorkspacePath,
};

/// A workspace whose engine is refusing a removal batch: sync stopped, and the
/// only way out is a human saying yes.
fn blocked_status() -> StatusCommandOutput {
    let snapshot = EngineSnapshot {
        revision: 4,
        phase: EnginePhase::Stalled,
        degradation: Degradation::MassDeletionBlocked {
            removals: 72,
            entries: 121,
        },
        refused_removals: Arc::new(BTreeSet::from([
            WorkspacePath::new("notes/a.md"),
            WorkspacePath::new("notes/b.md"),
        ])),
        ..EngineSnapshot::default()
    };
    let sources = BTreeMap::from([(
        StatusSource::Convergence,
        SourceRevision {
            source: StatusSource::Convergence,
            revision: StatusSourceRevision::new(4),
            observed_at: StatusTimestamp::new("2026-07-19T12:00:00Z"),
            freshness: SourceFreshness::Current,
        },
    )]);
    let source_facts = BTreeMap::from([(
        StatusSource::Convergence,
        StatusSourceFacts::Convergence(Box::new(engine_convergence_facts(&snapshot))),
    )]);
    super::reducer::reduce_projection_status(
        &healthy_status(),
        &sources,
        &source_facts,
        &StatusTimestamp::new("2026-07-19T12:00:00Z"),
    )
}

#[test]
fn a_blocked_deletion_names_its_counts_and_the_way_out() {
    let output = blocked_status();

    let fact = output
        .status_summary
        .facts
        .iter()
        .find(|fact| fact.kind.as_str() == "sync.mass_deletion_blocked")
        .expect("the blocked deletion has its own fact");
    assert_eq!(
        fact.parameters.get("removals").map(String::as_str),
        Some("72")
    );
    assert_eq!(
        fact.parameters.get("entries").map(String::as_str),
        Some("121")
    );
    assert_eq!(
        fact.parameters.get("threshold").map(String::as_str),
        Some("64")
    );
    assert_eq!(
        fact.action.as_ref().map(|action| action.kind.as_str()),
        Some("confirm-deletions")
    );

    // The old behaviour: one attention item saying sync "is needs attention at
    // revision N", and nothing to run. Both halves are asserted because either
    // one alone leaves the state unescapable.
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("72 of 121") && item.contains("bowline deletions")),
        "attention items: {:?}",
        output.status.attention_items
    );
    assert_eq!(
        output
            .next_actions
            .iter()
            .filter_map(|action| action.command.clone())
            .filter(|command| command.starts_with("bowline deletions"))
            .collect::<Vec<_>>(),
        vec![
            "bowline deletions".to_string(),
            "bowline deletions --confirm".to_string(),
        ],
        "status chains: name the problem, preview it, confirm it"
    );
    assert!(
        output
            .next_actions
            .iter()
            .any(
                |action| action.command.as_deref() == Some("bowline deletions --confirm")
                    && action.mutates
            ),
        "the confirmation is declared as mutating"
    );
}

#[test]
fn the_cli_overlay_carries_the_blocked_deletion_and_drops_it_when_it_clears() {
    let blocked = blocked_status();
    let mut cli = healthy_status();
    overlay_convergence_status(&mut cli, &blocked);

    assert!(
        cli.status_summary
            .facts
            .iter()
            .any(|fact| fact.kind.as_str() == "sync.mass_deletion_blocked"),
        "the CLI composes its own status and must not lose the daemon's block"
    );
    assert!(
        cli.next_actions
            .iter()
            .any(|action| action.command.as_deref() == Some("bowline deletions --confirm"))
    );

    // Once the batch publishes, every trace of it must go with it — a stale
    // "confirm the deletion" is an invitation to authorise nothing.
    let cleared = healthy_status();
    overlay_convergence_status(&mut cli, &cleared);
    assert!(
        cli.status_summary
            .facts
            .iter()
            .all(|fact| fact.kind.as_str() != "sync.mass_deletion_blocked")
    );
    assert!(
        cli.next_actions
            .iter()
            .all(|action| action.command.as_deref() != Some("bowline deletions --confirm"))
    );
}

fn healthy_status() -> StatusCommandOutput {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = std::fs::read_to_string(root.join("tests/contracts/status/healthy.json"))
        .expect("healthy status fixture");
    serde_json::from_str(&fixture).expect("typed healthy status fixture")
}

#[test]
fn project_convergence_replaces_every_workspace_convergence_surface() {
    let sibling = WorkspacePath::new("projects/sibling/file.txt");
    let snapshot = EngineSnapshot {
        revision: 43,
        phase: EnginePhase::Syncing,
        observed_ref: None,
        applied_manifest: None,
        pending_intents: 0,
        dirty: 1,
        dirty_paths: Arc::new(BTreeSet::from([sibling])),
        dirty_subtree_paths: Arc::new(BTreeSet::new()),
        pending_intent_paths: Arc::new(BTreeSet::new()),
        scan_required: false,
        unattributed_pull_pending: false,
        cycle_active: false,
        last_success_at: None,
        degradation: Degradation::Nominal,
        unsyncable: Arc::new(std::collections::BTreeMap::new()),
        refused_removals: Arc::new(BTreeSet::new()),
    };
    let global = engine_convergence_facts(&snapshot);
    let scoped =
        scoped_engine_convergence_facts(&snapshot, &WorkspacePath::new("projects/current"));
    assert!(!global.ready);
    assert!(scoped.ready);

    let sources = BTreeMap::from([(
        StatusSource::Convergence,
        SourceRevision {
            source: StatusSource::Convergence,
            revision: StatusSourceRevision::new(43),
            observed_at: StatusTimestamp::new("2026-07-19T12:00:00Z"),
            freshness: SourceFreshness::Current,
        },
    )]);
    let source_facts = BTreeMap::from([(
        StatusSource::Convergence,
        StatusSourceFacts::Convergence(Box::new(global)),
    )]);
    let mut output = super::reducer::reduce_projection_status(
        &healthy_status(),
        &sources,
        &source_facts,
        &StatusTimestamp::new("2026-07-19T12:00:00Z"),
    );
    assert_eq!(
        output.status.level,
        bowline_core::status::StatusLevel::Attention
    );
    assert!(!output.status.attention_items.is_empty());
    assert!(output.items.iter().any(|item| {
        item.subject
            .as_ref()
            .is_some_and(|subject| subject.id == "workspace-convergence")
    }));

    replace_convergence_status(&mut output, &scoped, "projects/current");

    assert_eq!(
        output.status.level,
        bowline_core::status::StatusLevel::Healthy
    );
    assert!(output.status.attention_items.is_empty());
    assert!(
        output
            .status_summary
            .facts
            .iter()
            .all(|fact| fact.id.as_str() != "workspace-convergence")
    );
    assert!(output.items.iter().all(|item| {
        item.subject
            .as_ref()
            .is_none_or(|subject| subject.id != "workspace-convergence")
    }));
    assert!(
        output
            .limits
            .iter()
            .all(|limit| limit.capability != "workspace-convergence")
    );
    assert_eq!(
        output
            .convergence
            .as_ref()
            .expect("project convergence")
            .state,
        bowline_core::status::ConvergenceReadinessState::Ready
    );
    assert_eq!(output.sync_queue.as_ref().expect("project queue").queued, 0);

    let relevant = scoped_engine_convergence_facts(
        &EngineSnapshot {
            dirty_paths: Arc::new(BTreeSet::from([WorkspacePath::new(
                "projects/current/file.txt",
            )])),
            ..snapshot
        },
        &WorkspacePath::new("projects/current"),
    );
    replace_convergence_status(&mut output, &relevant, "projects/current");
    let project_fact = output
        .status_summary
        .facts
        .iter()
        .find(|fact| fact.id.as_str() == "project-convergence")
        .expect("project convergence fact");
    assert_eq!(
        project_fact.scope,
        bowline_core::status::StatusFactScope::Project
    );
    assert_eq!(project_fact.kind.as_str(), "project.convergence");
    assert_eq!(project_fact.scope_id.as_deref(), Some("projects/current"));
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|summary| summary.starts_with("Project sync is syncing"))
    );
    assert_eq!(
        output.status.level,
        bowline_core::status::StatusLevel::Attention
    );

    let mut cli = healthy_status();
    overlay_convergence_status(&mut cli, &output);
    assert_eq!(
        cli.status.level,
        bowline_core::status::StatusLevel::Attention
    );
    assert!(
        cli.status_summary
            .facts
            .iter()
            .any(|fact| fact.id.as_str() == "project-convergence")
    );
    assert!(
        cli.status
            .attention_items
            .iter()
            .any(|summary| summary.starts_with("Project sync is syncing"))
    );

    let mut ready_source = output.clone();
    replace_convergence_status(&mut ready_source, &scoped, "projects/current");
    overlay_convergence_status(&mut cli, &ready_source);
    assert_eq!(cli.status.level, bowline_core::status::StatusLevel::Healthy);
    assert!(cli.status.attention_items.is_empty());
    assert!(
        cli.status_summary
            .facts
            .iter()
            .all(|fact| !fact.id.as_str().contains("convergence"))
    );
}
