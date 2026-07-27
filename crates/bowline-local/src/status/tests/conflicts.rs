use super::*;

use crate::sync::manifest_engine::CONFLICT_ASIDE_MARKER;

/// Seed a workspace whose accepted root is the temp directory itself, so the
/// conflict scan reads real files rather than a `~/Code` placeholder.
fn seeded_store(temp: &TempWorkspace, workspace_id: &WorkspaceId) -> MetadataStore {
    let store = MetadataStore::open(temp.root().join("local.sqlite3")).expect("store");
    store
        .insert_workspace(workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            workspace_id,
            &temp.root().display().to_string(),
            "2026-06-23T12:00:00Z",
        )
        .expect("root insert");
    store
}

fn status_for(temp: &TempWorkspace) -> bowline_core::commands::StatusCommandOutput {
    compose_status(StatusOptions {
        db_path: Some(temp.root().join("local.sqlite3")),
        requested_path: Some(temp.root().display().to_string()),
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes")
}

fn write(temp: &TempWorkspace, relative: &str, contents: &str) {
    let path = temp.root().join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, contents).expect("write file");
}

#[test]
fn a_conflict_aside_on_disk_becomes_a_status_attention_item() {
    let temp = TempWorkspace::new("status-conflict-attention").expect("temp workspace");
    let workspace_id = WorkspaceId::new("ws_conflict");
    let store = seeded_store(&temp, &workspace_id);
    drop(store);
    write(&temp, "acme/web/src/auth.ts", "local");
    write(
        &temp,
        &format!("acme/web/src/auth.ts{CONFLICT_ASIDE_MARKER}deadbeef"),
        "remote",
    );

    let status = status_for(&temp);

    assert_eq!(status.status.level, StatusLevel::Attention);
    assert!(
        status
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("acme/web/src/auth.ts")),
        "attention must name the real path: {:?}",
        status.status.attention_items,
    );
    let conflict = status
        .items
        .iter()
        .find(|item| item.kind == StatusItemKind::Conflict)
        .expect("a conflict status item");
    assert!(
        conflict.summary.contains("acme/web/src/auth.ts")
            && conflict.summary.contains(CONFLICT_ASIDE_MARKER),
        "the item must name the path pair: {}",
        conflict.summary,
    );
    assert!(
        status
            .status_summary
            .facts
            .iter()
            .any(|fact| fact.kind.as_str() == "sync.conflict_unresolved"),
        "the conflict must reach the fact reducer",
    );
    assert!(
        status.next_actions.iter().any(|action| action
            .command
            .as_deref()
            .is_some_and(|command| command.starts_with("bowline conflicts"))),
        "status must point at a command that exists: {:?}",
        status.next_actions,
    );
}

#[test]
fn a_workspace_without_asides_reports_no_conflict_attention() {
    let temp = TempWorkspace::new("status-conflict-clean").expect("temp workspace");
    let workspace_id = WorkspaceId::new("ws_clean");
    drop(seeded_store(&temp, &workspace_id));
    write(&temp, "acme/web/src/auth.ts", "local");

    let status = status_for(&temp);

    assert!(
        !status
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Conflict),
        "no aside on disk means no conflict item",
    );
}

#[test]
fn removing_the_aside_clears_the_conflict_from_status() {
    // The aside's absence is the resolution; nothing else records the state, so
    // status must stop reporting the moment the file is gone.
    let temp = TempWorkspace::new("status-conflict-cleared").expect("temp workspace");
    let workspace_id = WorkspaceId::new("ws_cleared");
    drop(seeded_store(&temp, &workspace_id));
    write(&temp, "notes.md", "local");
    let aside = format!("notes.md{CONFLICT_ASIDE_MARKER}deadbeef");
    write(&temp, &aside, "remote");
    assert!(
        status_for(&temp)
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Conflict)
    );

    std::fs::remove_file(temp.root().join(&aside)).expect("remove aside");

    assert!(
        !status_for(&temp)
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Conflict)
    );
}
