use super::*;

#[test]
fn accept_journal_rollback_restores_backups_and_removes_created_paths() {
    let temp = TempWorkspace::new("accept-journal-rollback").expect("temp");
    let main_root = temp.root().join("Code/apps/web");
    let namespace_root = temp.root().join("Code/.work/apps/web/review");
    fs::create_dir_all(main_root.join("src")).expect("main root");
    fs::write(main_root.join("src/index.ts"), "before\n").expect("main file");

    let mut journal = AcceptJournal::create(&namespace_root, &main_root).expect("journal");
    let backup_dir = journal.backup_dir().to_path_buf();
    journal
        .backup_existing(&main_root.join("src/index.ts"))
        .expect("backup");
    journal.record_created_dir(&main_root.join("generated"));
    fs::create_dir_all(main_root.join("generated")).expect("created dir");
    journal.record_created(&main_root.join("generated/new.ts"));
    fs::write(main_root.join("generated/new.ts"), "new\n").expect("created file");

    journal.rollback().expect("rollback");

    assert_eq!(
        fs::read_to_string(main_root.join("src/index.ts")).expect("restored"),
        "before\n"
    );
    assert!(!main_root.join("generated").exists());
    assert!(!backup_dir.exists());
}

#[test]
fn accept_journal_commit_removes_backup_dir_after_success() {
    let temp = TempWorkspace::new("accept-journal-commit").expect("temp");
    let main_root = temp.root().join("Code/apps/web");
    let namespace_root = temp.root().join("Code/.work/apps/web/review");
    fs::create_dir_all(&main_root).expect("main root");
    fs::write(main_root.join("index.ts"), "before\n").expect("main file");

    let mut journal = AcceptJournal::create(&namespace_root, &main_root).expect("journal");
    let backup_dir = journal.backup_dir().to_path_buf();
    journal
        .backup_existing(&main_root.join("index.ts"))
        .expect("backup");
    fs::write(main_root.join("index.ts"), "after\n").expect("new file");

    journal.commit().expect("commit");

    assert!(!backup_dir.exists());
    assert_eq!(
        fs::read_to_string(main_root.join("index.ts")).expect("accepted"),
        "after\n"
    );
}

#[test]
fn same_project_writer_lock_reports_contention() {
    let temp = TempWorkspace::new("work-view-writer-lock").expect("temp");
    let namespace_root = temp.root().join("Code/.work");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let first = ProjectWriterLock::acquire_with_timeout(
        &namespace_root,
        &workspace_id,
        &project_id,
        "apps/web",
        Duration::ZERO,
    )
    .expect("first lock");

    let error = ProjectWriterLock::acquire_with_timeout(
        &namespace_root,
        &workspace_id,
        &project_id,
        "apps/web",
        Duration::ZERO,
    )
    .expect_err("second lock should contend");

    assert!(matches!(error, WorkViewError::ProjectWriterBusy { .. }));
    drop(first);
    ProjectWriterLock::acquire_with_timeout(
        &namespace_root,
        &workspace_id,
        &project_id,
        "apps/web",
        Duration::ZERO,
    )
    .expect("lock released");
}

#[test]
fn different_project_writer_locks_do_not_block() {
    let temp = TempWorkspace::new("work-view-writer-lock-independent").expect("temp");
    let namespace_root = temp.root().join("Code/.work");
    let workspace_id = WorkspaceId::new("ws_code");
    let first = ProjectWriterLock::acquire_with_timeout(
        &namespace_root,
        &workspace_id,
        &ProjectId::new("proj_web"),
        "apps/web",
        Duration::ZERO,
    )
    .expect("first lock");
    let second = ProjectWriterLock::acquire_with_timeout(
        &namespace_root,
        &workspace_id,
        &ProjectId::new("proj_api"),
        "apps/api",
        Duration::ZERO,
    )
    .expect("second project lock");

    drop(second);
    drop(first);
}
