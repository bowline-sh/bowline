use super::*;

#[test]
fn materialize_snapshot_rejects_symlink_targets_outside_workspace() {
    for (name, target) in [
        (
            "sync-materialize-absolute-symlink",
            "/workspace/user/.ssh/config",
        ),
        ("sync-materialize-parent-symlink", ".."),
    ] {
        let workspace = TempWorkspace::new(name).expect("workspace");
        let snapshot = snapshot_with_symlink(WorkspaceId::new("ws_code"), "app/config", target);

        let error =
            materialize_snapshot(workspace.root(), None, &snapshot).expect_err("unsafe symlink");

        assert!(matches!(
            error,
            SyncRunnerError::UnsafeMaterializationPath(_)
        ));
        assert!(
            fs::symlink_metadata(workspace.root().join("app").join("config")).is_err(),
            "unsafe symlink target must not be materialized: {target:?}"
        );
    }
}
