use super::*;
use bowline_core::commands::{IndexSource, IndexState};
use bowline_index::{IndexReadiness, Language, SymbolKind};

#[test]
fn symbol_record_ids_are_workspace_scoped() {
    let document = IndexedDocument {
        path: "src/lib.ts".to_string(),
        absolute_path: PathBuf::from("/tmp/Code/apps/web/src/lib.ts"),
        body: "export function boot() { return true; }\n".to_string(),
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        hydration_state: HydrationState::Local,
    };
    let symbol = SymbolRecord {
        name: "boot".to_string(),
        kind: SymbolKind::Function,
        language: Language::TypeScript,
        path: document.path.clone(),
        project_id: "proj_web".to_string(),
        snapshot_id: "snap_web".to_string(),
        byte_range: 16..20,
        line_range: 1..1,
        parser_status: IndexReadiness::Ready,
        access: AccessFlags {
            policy_readable: true,
            lease_readable: true,
            generated: false,
            local_only: false,
        },
    };

    let first = indexed_project_for_symbol_id_test("ws_one");
    let second = indexed_project_for_symbol_id_test("ws_two");

    let first_record = symbol_record_to_store(&first, &document, symbol.clone(), 0, "now");
    let second_record = symbol_record_to_store(&second, &document, symbol, 0, "now");

    assert_ne!(first_record.id, second_record.id);
    assert!(first_record.id.contains("ws_one"));
    assert!(second_record.id.contains("ws_two"));
}

fn indexed_project_for_symbol_id_test(workspace_id: &str) -> IndexedProject {
    IndexedProject {
        workspace_id: WorkspaceId::new(workspace_id),
        project_id: ProjectId::new("proj_web"),
        root: PathBuf::from("/tmp/Code/apps/web"),
        requested_path: "/tmp/Code/apps/web".to_string(),
        snapshot_id: SnapshotId::new("snap_web"),
        text_index: TextIndex::new("now"),
        symbol_index: bowline_index::SymbolIndex::new("now"),
        documents: Vec::new(),
        index_status: IndexStatus {
            state: IndexState::Ready,
            source: IndexSource::Local,
            indexed_at: Some("now".to_string()),
            updated_at: Some("now".to_string()),
            snapshot_id: Some(SnapshotId::new("snap_web")),
            index_pack_object_key: None,
            path_count: 0,
            file_count: 0,
            indexed_bytes: 0,
            pending_path_count: None,
            degraded_reason: None,
            summary: "ready".to_string(),
            next_action: None,
        },
    }
}
