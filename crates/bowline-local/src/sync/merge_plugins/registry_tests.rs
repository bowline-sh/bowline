use std::{
    fs,
    sync::{Arc, Mutex},
};

use bowline_core::ids::{DeviceId, WorkspaceId};

use super::{
    CONFIG_FILE_NAME, ExternalMergeDecision, ExternalMergePlugin, LoadedMergePlugin,
    MAX_PLUGIN_MATCH_PATH_BYTES, MergePluginApprovalRecord, MergePluginIdentity,
    MergePluginRegistry, blake3_digest, config::ProjectMergePluginConfig,
    matcher::policy_bound_matcher_version,
};
use crate::workspace::TempWorkspace;

#[test]
fn approved_project_wasm_plugin_merges_and_records_audit() {
    let workspace = TempWorkspace::new("merge-plugin-registry").expect("workspace");
    fs::create_dir_all(workspace.root().join(".bowline/plugins")).expect("plugin dir");
    let module = wat::parse_str(
        r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param $base_ptr i32) (param $base_len i32)
    (param $local_ptr i32) (param $local_len i32)
    (param $remote_ptr i32) (param $remote_len i32)
    (param $path_ptr i32) (param $path_len i32)
    (result i64)
    i32.const 4096
    i32.const 42
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param $ptr i32) (param $len i32) (param $path_ptr i32) (param $path_len i32)
    (result i32)
    local.get $len
    i32.const 1
    i32.eq))
"#,
    )
    .expect("wat parses");
    let digest = blake3_digest(&module);
    fs::write(
        workspace.root().join(".bowline/plugins/binary-merge.wasm"),
        &module,
    )
    .expect("plugin module");
    fs::write(
        workspace.root().join(CONFIG_FILE_NAME),
        format!(
            r#"
schema = 1

[[plugins]]
id = "binary-merge"
version = "1.0.0"
digest = "{digest}"
module = ".bowline/plugins/binary-merge.wasm"
match = ["*.bin"]
"#
        ),
    )
    .expect("config");
    let plugin = MergePluginIdentity {
        id: "binary-merge".to_string(),
        version: "1.0.0".to_string(),
        digest,
        matcher_version: policy_bound_matcher_version("2", &["*.bin".to_string()]),
        validator_version: "1".to_string(),
    };
    let registry = MergePluginRegistry::load_project(
        workspace.root(),
        &WorkspaceId::new("ws_code"),
        &[MergePluginApprovalRecord {
            workspace_id: WorkspaceId::new("ws_code"),
            plugin: plugin.clone(),
            state: "approved".to_string(),
            approved_by_device_id: DeviceId::new("device_local"),
            approved_at: "2026-07-02T10:00:00Z".to_string(),
        }],
    )
    .expect("registry");
    assert!(registry.approval_requests.is_empty());
    let module_before = ready_module(&registry.registry, "binary-merge");

    match registry.registry.merge_external(
        "image.bin",
        &[0, 1, 2, 3],
        &[0, 1, 255, 3],
        &[0, 1, 254, 3],
    ) {
        ExternalMergeDecision::Merged(bytes) => assert_eq!(bytes, b"*"),
        decision => panic!("expected validated merge, got {decision:?}"),
    }
    match registry.registry.merge_external(
        "second.bin",
        &[0, 1, 2, 3],
        &[0, 1, 255, 3],
        &[0, 1, 254, 3],
    ) {
        ExternalMergeDecision::Merged(bytes) => assert_eq!(bytes, b"*"),
        decision => panic!("expected second validated merge, got {decision:?}"),
    }
    let module_after = ready_module(&registry.registry, "binary-merge");
    assert!(Arc::ptr_eq(&module_before, &module_after));
    let audit = registry.registry.take_audit_records();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].path, "image.bin");
    assert_eq!(audit[1].path, "second.bin");
    assert_eq!(audit[0].plugin, plugin);
    assert_eq!(audit[1].plugin, plugin);
}

fn ready_module(registry: &MergePluginRegistry, plugin_id: &str) -> Arc<wasmtime::Module> {
    registry
        .external
        .iter()
        .find(|plugin| plugin.identity.id == plugin_id)
        .and_then(|plugin| match &plugin.loaded {
            LoadedMergePlugin::Ready(module) => Some(Arc::clone(module)),
            LoadedMergePlugin::UnsupportedMatcherContract { .. } => None,
            LoadedMergePlugin::Unavailable(_) => None,
        })
        .expect("approved plugin module is loaded")
}

#[test]
fn approved_project_wasm_plugin_cannot_bypass_builtin_validation() {
    let workspace = TempWorkspace::new("merge-plugin-json-validator-lie").expect("workspace");
    fs::create_dir_all(workspace.root().join(".bowline/plugins")).expect("plugin dir");
    let module = wat::parse_str(
        r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i32.const 4096
    i32.const 120
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    i32.const 1))
"#,
    )
    .expect("wat parses");
    let digest = blake3_digest(&module);
    fs::write(workspace.root().join(".bowline/plugins/json.wasm"), &module).expect("plugin module");
    fs::write(
        workspace.root().join(CONFIG_FILE_NAME),
        format!(
            r#"
schema = 1

[[plugins]]
id = "json"
version = "1.0.0"
digest = "{digest}"
module = ".bowline/plugins/json.wasm"
match = ["*.json"]
"#
        ),
    )
    .expect("config");
    let plugin = MergePluginIdentity {
        id: "json".to_string(),
        version: "1.0.0".to_string(),
        digest,
        matcher_version: policy_bound_matcher_version("2", &["*.json".to_string()]),
        validator_version: "1".to_string(),
    };
    let registry = MergePluginRegistry::load_project(
        workspace.root(),
        &WorkspaceId::new("ws_code"),
        &[MergePluginApprovalRecord {
            workspace_id: WorkspaceId::new("ws_code"),
            plugin,
            state: "approved".to_string(),
            approved_by_device_id: DeviceId::new("device_local"),
            approved_at: "2026-07-02T10:00:00Z".to_string(),
        }],
    )
    .expect("registry");

    match registry
        .registry
        .merge_external("package.json", b"{}", b"{\"a\":1}", b"{\"a\":2}")
    {
        ExternalMergeDecision::Conflict(reason) => {
            assert!(reason.contains("invalid output"));
        }
        decision => panic!("expected built-in validation conflict, got {decision:?}"),
    }
}

#[test]
fn future_matcher_contracts_with_unknown_syntax_conflict_conservatively() {
    let workspace = TempWorkspace::new("merge-plugin-future-syntax").expect("workspace");
    fs::write(
        workspace.root().join(CONFIG_FILE_NAME),
        r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["{analysis,run}.ipynb"]
matcher-version = "3"
"#,
    )
    .expect("write config");

    let workspace_id = WorkspaceId::new("ws_plugins");
    let plugins = MergePluginRegistry::load_project(workspace.root(), &workspace_id, &[])
        .expect("project registry loads");

    match plugins
        .registry
        .merge_external("run.ipynb", b"base", b"local", b"remote")
    {
        ExternalMergeDecision::Conflict(reason) => {
            assert!(reason.contains("unsupported matcher contract `3`"));
        }
        decision => panic!("expected conservative conflict, got {decision:?}"),
    }
    assert!(matches!(
        plugins
            .registry
            .merge_external("notes.txt", b"base", b"local", b"remote"),
        ExternalMergeDecision::NoMatch
    ));
    let oversized_path = format!("{}run.ipynb", "a".repeat(MAX_PLUGIN_MATCH_PATH_BYTES + 1));
    match plugins
        .registry
        .merge_external(&oversized_path, b"base", b"local", b"remote")
    {
        ExternalMergeDecision::Conflict(reason) => {
            assert!(reason.contains("exceeds 1024 bytes"));
            assert!(reason.contains("notebooks"));
        }
        decision => panic!("expected oversized conservative conflict, got {decision:?}"),
    }
}

#[test]
fn plugin_matching_conflicts_before_matching_oversized_paths() {
    let config = ProjectMergePluginConfig::parse(
        r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
"#,
    )
    .expect("config parses");
    let declaration = &config.plugins[0];
    let registry = MergePluginRegistry {
        external: vec![ExternalMergePlugin {
            identity: declaration.identity(),
            patterns: declaration.patterns.clone(),
            approved_on_device: false,
            loaded: LoadedMergePlugin::Unavailable("module file is missing".to_string()),
        }],
        wasm_engine: None,
        audit: Mutex::new(Vec::new()),
    };
    let oversized_path = format!("{}.ipynb", "a".repeat(MAX_PLUGIN_MATCH_PATH_BYTES + 1));

    match registry.merge_external(&oversized_path, b"{}", b"{}", b"{}") {
        ExternalMergeDecision::Conflict(reason) => {
            assert!(reason.contains("exceeds 1024 bytes"));
        }
        decision => panic!("expected oversized path conflict, got {decision:?}"),
    }
    let unrelated_oversized_path = format!("{}.txt", "a".repeat(MAX_PLUGIN_MATCH_PATH_BYTES + 1));
    assert!(matches!(
        registry.merge_external(&unrelated_oversized_path, b"{}", b"{}", b"{}"),
        ExternalMergeDecision::NoMatch
    ));
}
