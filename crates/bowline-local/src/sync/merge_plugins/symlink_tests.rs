use super::*;
use crate::{sync::merge_plugins::matcher::policy_bound_matcher_version, workspace::TempWorkspace};

#[cfg(unix)]
#[test]
fn approved_project_plugin_rejects_module_symlink_outside_workspace() {
    let workspace = TempWorkspace::new("merge-plugin-symlink").expect("workspace");
    let external = TempWorkspace::new("merge-plugin-external").expect("workspace");
    fs::create_dir_all(workspace.root().join(".bowline/plugins")).expect("plugin dir");
    let module = b"not wasm".to_vec();
    let digest = blake3_digest(&module);
    let external_module = external.root().join("external.wasm");
    fs::write(&external_module, &module).expect("external module");
    std::os::unix::fs::symlink(
        &external_module,
        workspace.root().join(".bowline/plugins/escape.wasm"),
    )
    .expect("module symlink");
    fs::write(
        workspace.root().join(CONFIG_FILE_NAME),
        format!(
            r#"
schema = 1

[[plugins]]
id = "escape"
version = "1.0.0"
digest = "{digest}"
module = ".bowline/plugins/escape.wasm"
match = ["*.bin"]
"#
        ),
    )
    .expect("config");
    let plugin = MergePluginIdentity {
        id: "escape".to_string(),
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
            plugin,
            state: "approved".to_string(),
            approved_by_device_id: DeviceId::new("device_local"),
            approved_at: "2026-07-02T10:00:00Z".to_string(),
        }],
    )
    .expect("registry");

    match registry
        .registry
        .merge_external("image.bin", b"base", b"local", b"remote")
    {
        ExternalMergeDecision::Conflict(reason) => {
            assert!(reason.contains("outside workspace"));
        }
        decision => panic!("expected unavailable plugin conflict, got {decision:?}"),
    }
}
