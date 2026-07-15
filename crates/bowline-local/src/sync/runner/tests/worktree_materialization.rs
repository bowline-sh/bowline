use super::*;

#[test]
fn materialization_preserves_out_of_root_worktree_registration_on_removal() {
    let workspace =
        TempWorkspace::new("sync-materialize-preserve-external-registration").expect("workspace");
    let registration = workspace.root().join("repo/.git/worktrees/feat");
    fs::create_dir_all(registration.join("refs/heads")).expect("registration refs");
    fs::write(registration.join("gitdir"), b"/elsewhere/repo/feat/.git\n").expect("gitdir");
    fs::write(registration.join("HEAD"), b"ref: refs/heads/feat\n").expect("HEAD");
    fs::write(
        registration.join("refs/heads/feat"),
        b"0123456789012345678901234567890123456789\n",
    )
    .expect("ref");
    let base = snapshot_with_files(
        WorkspaceId::new("ws_code"),
        &[
            (
                "repo/.git/worktrees/feat/gitdir",
                b"/elsewhere/repo/feat/.git\n".as_slice(),
            ),
            (
                "repo/.git/worktrees/feat/HEAD",
                b"ref: refs/heads/feat\n".as_slice(),
            ),
            (
                "repo/.git/worktrees/feat/refs/heads/feat",
                b"0123456789012345678901234567890123456789\n".as_slice(),
            ),
        ],
    );
    let target = snapshot_with_files(WorkspaceId::new("ws_code"), &[]);

    materialize_snapshot(workspace.root(), Some(&base), &target).expect("materialize");

    assert_eq!(
        fs::read(registration.join("gitdir")).expect("gitdir"),
        b"/elsewhere/repo/feat/.git\n"
    );
    assert_eq!(
        fs::read(registration.join("HEAD")).expect("HEAD"),
        b"ref: refs/heads/feat\n"
    );
}

#[test]
fn materialization_still_deletes_in_root_worktree_registration_on_removal() {
    let workspace =
        TempWorkspace::new("sync-materialize-delete-in-root-registration").expect("workspace");
    let registration = workspace.root().join("repo/.git/worktrees/feat");
    let gitdir_bytes = format!(
        "{}/repo/.claude/worktrees/feat/.git\n",
        workspace.root().display()
    );
    fs::create_dir_all(&registration).expect("registration");
    fs::write(registration.join("gitdir"), gitdir_bytes.as_bytes()).expect("gitdir");
    fs::write(
        registration.join("HEAD"),
        b"ref: refs/heads/worktree-feat\n",
    )
    .expect("HEAD");
    let base = snapshot_with_files(
        WorkspaceId::new("ws_code"),
        &[
            ("repo/.git/worktrees/feat/gitdir", gitdir_bytes.as_bytes()),
            (
                "repo/.git/worktrees/feat/HEAD",
                b"ref: refs/heads/worktree-feat\n".as_slice(),
            ),
        ],
    );
    let target = snapshot_with_files(WorkspaceId::new("ws_code"), &[]);

    materialize_snapshot(workspace.root(), Some(&base), &target).expect("materialize");

    assert!(
        fs::symlink_metadata(registration.join("gitdir")).is_err(),
        "portable registration gitdir removals should still propagate"
    );
    assert!(
        fs::symlink_metadata(registration.join("HEAD")).is_err(),
        "portable registration HEAD removals should still propagate"
    );
}

#[test]
fn external_git_worktree_registration_stays_machine_local() {
    let source = TempWorkspace::new("sync-external-worktree-source").expect("source");
    let destination =
        TempWorkspace::new("sync-external-worktree-destination").expect("destination");
    let external = TempWorkspace::new("sync-external-worktree-outside").expect("external");
    let source_root = fs::canonicalize(source.root()).expect("canonical source");
    let destination_root = fs::canonicalize(destination.root()).expect("canonical destination");
    let repo = source_root.join("repo");
    let external_worktree = external
        .root()
        .join(".t3")
        .join("worktrees")
        .join("repo")
        .join("feat");
    fs::create_dir_all(&repo).expect("repo dir");
    fs::create_dir_all(external_worktree.parent().expect("external parent"))
        .expect("external parent");
    assert_command_success(
        Command::new("git").arg("init").arg(&repo).output(),
        "git init",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=bowline@example.test"])
            .args(["-c", "user.name=Bowline Test"])
            .args(["commit", "--allow-empty", "-m", "initial"])
            .output(),
        "git commit",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "feat"])
            .arg(&external_worktree)
            .output(),
        "git worktree add",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["update-ref", "refs/t3/checkpoints/dGhyZWFk/turn/1", "HEAD"])
            .output(),
        "git update-ref checkpoint",
    );
    let registration = repo.join(".git").join("worktrees").join("feat");
    fs::write(registration.join("codex-thread.json"), br#"{"version":1}"#).expect("codex metadata");

    let workspace_id = WorkspaceId::new("ws_code");
    let candidate_a = super::super::super::coalescer::coalesce_workspace_scan(
        &source_root,
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [44_u8; 32],
        "2026-07-08T10:30:00Z",
    )
    .expect("coalesce external worktree");
    materialize_snapshot(&destination_root, None, &candidate_a.snapshot).expect("materialize");

    let destination_registration = destination_root
        .join("repo")
        .join(".git")
        .join("worktrees")
        .join("feat");
    assert!(
        fs::symlink_metadata(destination_registration.join("gitdir")).is_err(),
        "out-of-root registration gitdir should not materialize on another root"
    );
    assert!(
        fs::symlink_metadata(destination_registration.join("codex-thread.json")).is_err(),
        "foreign metadata inside out-of-root registrations should stay local"
    );
    let checkpoint_refs = Command::new("git")
        .arg("-C")
        .arg(destination_root.join("repo"))
        .args(["for-each-ref", "refs/t3/"])
        .output()
        .expect("checkpoint refs");
    assert!(
        checkpoint_refs.status.success(),
        "checkpoint refs command failed: {}",
        String::from_utf8_lossy(&checkpoint_refs.stderr)
    );
    assert!(
        String::from_utf8_lossy(&checkpoint_refs.stdout).contains("refs/t3/checkpoints"),
        "checkpoint refs should still follow opaque git state"
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(destination_root.join("repo"))
            .args(["checkout", "feat"])
            .output(),
        "destination checkout feat",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(destination_root.join("repo"))
            .args(["worktree", "prune", "--expire=now"])
            .output(),
        "destination prune",
    );

    let candidate_b = super::super::super::coalescer::coalesce_workspace_scan(
        &destination_root,
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_remote"),
        [44_u8; 32],
        "2026-07-08T10:31:00Z",
    )
    .expect("coalesce destination");
    materialize_snapshot(
        &source_root,
        Some(&candidate_a.snapshot),
        &candidate_b.snapshot,
    )
    .expect("materialize back");

    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&external_worktree)
            .args(["status", "--short"])
            .output(),
        "origin external worktree remains usable",
    );
}

#[test]
fn claude_code_style_nested_worktree_follows_workspace() {
    let source = TempWorkspace::new("sync-claude-worktree-source").expect("source");
    let destination = TempWorkspace::new("sync-claude-worktree-destination").expect("destination");
    let source_root = fs::canonicalize(source.root()).expect("canonical source");
    let destination_root = fs::canonicalize(destination.root()).expect("canonical destination");
    let repo = source_root.join("repo");
    let worktree = repo.join(".claude").join("worktrees").join("feat");
    fs::create_dir_all(&repo).expect("repo dir");
    fs::create_dir_all(worktree.parent().expect("worktree parent")).expect("worktree parent");
    assert_command_success(
        Command::new("git").arg("init").arg(&repo).output(),
        "git init",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=bowline@example.test"])
            .args(["-c", "user.name=Bowline Test"])
            .args(["commit", "--allow-empty", "-m", "initial"])
            .output(),
        "git commit",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "worktree-feat"])
            .arg(&worktree)
            .output(),
        "git worktree add",
    );

    let workspace_id = WorkspaceId::new("ws_code");
    let candidate = super::super::super::coalescer::coalesce_workspace_scan(
        &source_root,
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [45_u8; 32],
        "2026-07-08T10:32:00Z",
    )
    .expect("coalesce claude worktree");

    materialize_snapshot(&destination_root, None, &candidate.snapshot).expect("materialize");

    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(destination_root.join("repo/.claude/worktrees/feat"))
            .args(["status", "--short"])
            .output(),
        "materialized nested worktree status",
    );
    let expected_gitdir = format!(
        "{}/repo/.claude/worktrees/feat/.git\n",
        destination_root.display()
    );
    let worktree_admin_dir = destination_root.join("repo/.git/worktrees");
    let gitdir_paths = fs::read_dir(&worktree_admin_dir)
        .expect("worktree admin dir")
        .map(|entry| entry.expect("worktree admin entry").path().join("gitdir"))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    assert_eq!(
        gitdir_paths.len(),
        1,
        "expected exactly one nested worktree registration"
    );
    assert_eq!(
        fs::read(&gitdir_paths[0]).expect("gitdir"),
        expected_gitdir.as_bytes()
    );
}
