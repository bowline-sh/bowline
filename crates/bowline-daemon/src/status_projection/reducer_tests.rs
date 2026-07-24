use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use bowline_core::commands::StatusCommandOutput;
use bowline_core::status::overlay_convergence_status;
use bowline_local::sync::manifest_engine::{
    Degradation, EnginePhase, EngineSnapshot, WorkspacePath,
};

use super::*;

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
