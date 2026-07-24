use std::{collections::BTreeSet, process::Command};

use bowline_core::status::GitObserverState;
use bowline_core::{
    ids::ProjectId, policy::MaterializationMode, status::ObservedWorkspaceSummary,
    workspace_graph::FileExecutability,
};

use crate::policy::classify_path_with_builtin_policy;

use super::{
    PathObservation, ProjectHealthDepth, ProjectObservation, ScanReport, attach_project_ids,
    git::read_git_ref, merge_scoped_and_shallow_reports, read_project_health, scan_workspace,
    scan_workspace_root_shallow, scan_workspace_scoped, scan_workspace_with_checkpoint,
};

#[test]
fn scan_stops_within_one_directory_entry_checkpoint() {
    let temp = crate::workspace::TempWorkspace::new("scan-cancellation-checkpoint")
        .expect("temp workspace");
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        std::fs::write(temp.root().join(name), name).expect("fixture");
    }
    let mut checkpoints = 0_u32;
    let error = scan_workspace_with_checkpoint(temp.root(), || {
        checkpoints += 1;
        if checkpoints > 2 {
            Err(super::ScanError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            )))
        } else {
            Ok(())
        }
    })
    .expect_err("scan cancellation");

    assert_eq!(
        checkpoints, 3,
        "no additional entries run after cancellation"
    );
    assert!(
        matches!(error, super::ScanError::Io(error) if error.kind() == std::io::ErrorKind::Interrupted)
    );
}

#[test]
fn nearest_project_assignment_unchanged() {
    let root = ProjectObservation {
        id: ProjectId::new("proj_root"),
        path: String::new(),
        has_git_repo: false,
        has_remote: false,
        stale_remote_tracking: false,
        untracked_file_count: 0,
        observer_state: GitObserverState::Ok,
        health_refresh_needed: false,
    };
    let parent = ProjectObservation {
        id: ProjectId::new("proj_a"),
        path: "a".to_string(),
        has_git_repo: false,
        has_remote: false,
        stale_remote_tracking: false,
        untracked_file_count: 0,
        observer_state: GitObserverState::Ok,
        health_refresh_needed: false,
    };
    let child = ProjectObservation {
        id: ProjectId::new("proj_ab"),
        path: "a/b".to_string(),
        has_git_repo: false,
        has_remote: false,
        stale_remote_tracking: false,
        untracked_file_count: 0,
        observer_state: GitObserverState::Ok,
        health_refresh_needed: false,
    };
    let mut report = ScanReport {
        root: std::path::PathBuf::new(),
        projects: vec![root, parent, child],
        paths: vec![
            path_observation("a/b/src/main.rs"),
            path_observation("z/readme.md"),
        ],
        summary: ObservedWorkspaceSummary::default(),
    };

    attach_project_ids(&mut report);

    assert_eq!(
        report.paths[0].project_id.as_ref().map(ProjectId::as_str),
        Some("proj_ab")
    );
    assert_eq!(
        report.paths[1].project_id.as_ref().map(ProjectId::as_str),
        Some("proj_root")
    );
}

#[test]
fn scan_observes_projects_without_mutating_git() {
    let temp = crate::workspace::TempWorkspace::new("scan-git").expect("temp workspace");
    temp.write_project_file("apps/web", "package.json", b"{}")
        .expect("package json");
    temp.create_git_repo("apps/web").expect("git repo");
    let detector = temp.mutation_detector().expect("mutation detector");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    detector.assert_unchanged().expect("scan should not mutate");
    assert_eq!(report.projects.len(), 1);
    assert_eq!(report.summary.repo_count, 1);
    assert_eq!(report.summary.no_remote_repo_count, 1);
    assert!(
        report
            .paths
            .iter()
            .any(|path| path.path == "apps/web/.git/config")
    );
}

#[test]
fn packed_ref_resolves_when_loose_ref_absent() {
    let temp = crate::workspace::TempWorkspace::new("scan-packed-ref").expect("temp workspace");
    let git_dir = temp.root().join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    std::fs::write(
        git_dir.join("packed-refs"),
        "# pack-refs\n0123456789abcdef0123456789abcdef01234567 refs/heads/main\n",
    )
    .expect("packed refs");

    let value = read_git_ref(&git_dir, "refs/heads/main").expect("packed ref read");

    assert_eq!(
        value.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}

#[test]
fn index_v4_marks_partial_not_all_untracked() {
    let temp = crate::workspace::TempWorkspace::new("scan-index-v4").expect("temp workspace");
    std::fs::write(temp.root().join("package.json"), b"{}").expect("package json");
    let git_dir = temp.root().join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    std::fs::write(git_dir.join("index"), minimal_git_index_header(4)).expect("v4 index");
    std::fs::write(temp.root().join("tracked.rs"), b"fn main() {}\n").expect("tracked file");

    let report = scan_workspace(temp.root()).expect("scan");
    let project = report
        .projects
        .iter()
        .find(|project| project.path.is_empty())
        .expect("root project");

    assert_eq!(project.observer_state, GitObserverState::Partial);
    assert_eq!(project.untracked_file_count, 0);
    assert_eq!(report.summary.git_partial_project_count, 1);
    assert_eq!(report.summary.git_unavailable_project_count, 0);
}

#[test]
fn git_config_read_error_marks_unavailable() {
    let temp = crate::workspace::TempWorkspace::new("scan-config-error").expect("temp workspace");
    let git_dir = temp.root().join(".git");
    std::fs::create_dir_all(git_dir.join("config")).expect("config dir");

    let health = read_project_health(&git_dir, temp.root(), ProjectHealthDepth::CheapIdentity);

    assert_eq!(health.observer_state, GitObserverState::Unavailable);
    assert!(!health.has_remote);
    assert!(health.health_refresh_needed);
}

#[test]
fn missing_config_is_ok_no_remote() {
    let temp = crate::workspace::TempWorkspace::new("scan-missing-config").expect("temp workspace");
    let git_dir = temp.root().join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");

    let health = read_project_health(&git_dir, temp.root(), ProjectHealthDepth::CheapIdentity);

    assert_eq!(health.observer_state, GitObserverState::Ok);
    assert!(!health.has_remote);
}

#[test]
fn cheap_identity_tick_stays_ok_not_partial() {
    let temp = crate::workspace::TempWorkspace::new("scan-cheap-ok").expect("temp workspace");
    let git_dir = temp.root().join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    std::fs::write(git_dir.join("index"), minimal_git_index_header(4)).expect("v4 index");
    std::fs::write(temp.root().join("file.rs"), b"fn main() {}\n").expect("file");

    let health = read_project_health(&git_dir, temp.root(), ProjectHealthDepth::CheapIdentity);

    assert_eq!(health.observer_state, GitObserverState::Ok);
    assert_eq!(health.untracked_file_count, 0);
    assert!(health.health_refresh_needed);
}

#[cfg(unix)]
#[test]
fn scan_captures_executable_bit_for_files() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let temp = crate::workspace::TempWorkspace::new("scan-executable").expect("workspace");
    let executable = temp.root().join("scripts/dev.sh");
    let regular = temp.root().join("README.md");
    fs::create_dir_all(executable.parent().expect("script parent")).expect("script parent");
    fs::write(&executable, b"#!/bin/sh\n").expect("script");
    fs::write(&regular, b"readme\n").expect("readme");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("chmod script");
    fs::set_permissions(&regular, fs::Permissions::from_mode(0o644)).expect("chmod readme");
    std::os::unix::fs::symlink("dev.sh", temp.root().join("scripts/dev-link")).expect("symlink");

    let report = scan_workspace(temp.root()).expect("scan");

    assert_eq!(
        executability_for_path(&report, "scripts/dev.sh"),
        FileExecutability::Executable
    );
    assert_eq!(
        executability_for_path(&report, "README.md"),
        FileExecutability::Regular
    );
    assert_eq!(
        executability_for_path(&report, "scripts"),
        FileExecutability::Regular
    );
    assert_eq!(
        executability_for_path(&report, "scripts/dev-link"),
        FileExecutability::Regular
    );
}

#[test]
fn scoped_scan_records_dirty_directory_root() {
    let temp = crate::workspace::TempWorkspace::new("scan-scoped-dir-root").expect("workspace");
    std::fs::create_dir_all(temp.root().join("app/src")).expect("source parent");
    std::fs::write(temp.root().join("app/src/main.rs"), b"fn main() {}\n").expect("source");
    let roots = BTreeSet::from(["app/src".to_string()]);

    let report = scan_workspace_scoped(temp.root(), &roots).expect("scoped scan");
    let paths = report
        .paths
        .iter()
        .map(|path| path.path.as_str())
        .collect::<Vec<_>>();

    assert!(
        paths.contains(&"app/src"),
        "dirty directory root should remain in scoped scan report"
    );
    assert!(paths.contains(&"app/src/main.rs"));
}

#[cfg(unix)]
#[test]
fn scan_normalizes_setuid_and_group_bits_to_plain_executable() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let temp =
        crate::workspace::TempWorkspace::new("scan-executable-normalized").expect("workspace");
    let path = temp.root().join("bin/tool");
    fs::create_dir_all(path.parent().expect("tool parent")).expect("tool parent");
    fs::write(&path, b"tool\n").expect("tool");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o4711)).expect("chmod tool");

    let report = scan_workspace(temp.root()).expect("scan");

    assert_eq!(
        executability_for_path(&report, "bin/tool"),
        FileExecutability::Executable
    );
}

#[test]
fn scan_counts_generated_env_and_dependency_paths() {
    let temp = crate::workspace::TempWorkspace::new("scan-policy").expect("temp workspace");
    temp.write_project_file("apps/web", "package.json", b"{}")
        .expect("package json");
    temp.write_project_file("apps/web", ".env.local", b"SECRET=value\n")
        .expect("env file");
    temp.create_generated_folder("apps/web", ".next")
        .expect("generated folder");
    std::fs::create_dir_all(temp.root().join("apps/web/node_modules/react")).expect("node modules");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert_eq!(report.summary.env_file_count, 1);
    assert!(report.summary.generated_path_count >= 1);
    assert!(report.summary.dependency_path_count >= 1);
}

#[test]
fn scan_counts_untracked_files_without_mutating_git() {
    let temp = crate::workspace::TempWorkspace::new("scan-untracked").expect("temp workspace");
    let repo = temp.root().join("apps").join("web");
    std::fs::create_dir_all(&repo).expect("repo dir");
    Command::new("git")
        .arg("init")
        .arg(&repo)
        .output()
        .expect("git init should run");
    Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["remote", "add", "origin", "git@example.com:acme/web.git"])
        .output()
        .expect("git remote should run");
    std::fs::write(repo.join("package.json"), b"{}").expect("package json");
    std::fs::create_dir_all(repo.join("notes")).expect("notes dir");
    std::fs::write(repo.join("notes").join("repro.md"), b"steps\n").expect("untracked file");
    std::fs::create_dir_all(repo.join("node_modules/react/.git")).expect("nested dependency");
    let detector =
        crate::workspace::WorkspaceMutationDetector::new(&repo).expect("mutation detector");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    detector.assert_unchanged().expect("scan should not mutate");
    assert_eq!(report.summary.repo_count, 1);
    assert_eq!(report.summary.no_remote_repo_count, 0);
    assert!(report.summary.untracked_file_count >= 1);
    assert!(
        report
            .projects
            .iter()
            .all(|project| project.path != "apps/web/node_modules/react")
    );
    assert!(report.paths.iter().any(|path| {
        path.path == "apps/web/notes/repro.md"
            && serde_json::to_value(path.policy.mode).unwrap() == "workspace-sync"
    }));
}

#[test]
fn scan_detects_stale_remote_tracking_refs_without_running_git() {
    let temp = crate::workspace::TempWorkspace::new("scan-stale-git-ref").expect("temp workspace");
    let git_dir = temp.root().join("apps/web/.git");
    std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
    std::fs::create_dir_all(git_dir.join("refs/remotes/origin")).expect("remote refs");
    std::fs::write(temp.root().join("apps/web/package.json"), b"{}").expect("package");
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
    std::fs::write(git_dir.join("config"), b"[remote \"origin\"]\n").expect("config");
    std::fs::write(git_dir.join("refs/heads/main"), b"aaaaaaaa\n").expect("local ref");
    std::fs::write(git_dir.join("refs/remotes/origin/main"), b"bbbbbbbb\n").expect("remote ref");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert_eq!(report.summary.repo_count, 1);
    assert_eq!(report.summary.stale_remote_tracking_repo_count, 1);
}

#[test]
fn scan_observes_bounded_git_transients_as_local_only() {
    let temp = crate::workspace::TempWorkspace::new("scan-git-transients").expect("temp workspace");
    temp.write_project_file("apps/web", "package.json", b"{}")
        .expect("package json");
    let git = temp.create_git_repo("apps/web").expect("git repo");
    std::fs::write(git.join("index.lock"), b"lock").expect("index lock");
    std::fs::write(git.join("gc.log"), b"gc").expect("gc log");
    std::fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
    std::fs::write(git.join("objects").join("pack").join("tmp_pack"), b"tmp").expect("tmp pack");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    for path in [
        "apps/web/.git/index.lock",
        "apps/web/.git/gc.log",
        "apps/web/.git/objects/pack/tmp_pack",
    ] {
        assert!(report.paths.iter().any(|observed| {
            observed.path == path
                && serde_json::to_value(observed.policy.mode).unwrap() == "local-only"
        }));
    }
    assert!(report.summary.local_only_path_count >= 3);
}

#[test]
fn nested_bare_exclusion_stays_local_only() {
    let temp = crate::workspace::TempWorkspace::new("scan-nested-bare-exclude").expect("workspace");
    temp.write_file(".bowlineignore", b"secret.txt\n")
        .expect("policy");
    temp.write_file("secret.txt", b"top secret\n")
        .expect("root secret");
    temp.write_file("deep/nested/secret.txt", b"nested secret\n")
        .expect("nested secret");

    let report = scan_workspace(temp.root()).expect("scan succeeds");
    let nested = report
        .paths
        .iter()
        .find(|observed| observed.path == "deep/nested/secret.txt")
        .expect("nested secret observed");

    assert_eq!(
        serde_json::to_value(nested.policy.classification).unwrap(),
        "local-only"
    );
    assert_eq!(nested.policy.mode, MaterializationMode::Ignore);
    assert!(report.summary.local_only_path_count >= 2);
}

#[test]
fn scanner_does_not_restore_dependency_paths_with_slash_free_include() {
    let temp = crate::workspace::TempWorkspace::new("scan-slash-free-include").expect("workspace");
    temp.write_file(".bowlineignore", b"node_modules\n!kept.js\n")
        .expect("policy");
    temp.write_file("node_modules/deep/kept.js", b"keep me\n")
        .expect("included dependency file");
    temp.write_file("node_modules/deep/drop.js", b"drop me\n")
        .expect("excluded dependency file");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert!(
        !report
            .paths
            .iter()
            .any(|observed| observed.path == "node_modules/deep/kept.js")
    );
}

#[test]
fn scan_recurses_git_refs_objects_and_packfiles_as_opaque_workspace_state() {
    let temp =
        crate::workspace::TempWorkspace::new("scan-git-opaque-tree").expect("temp workspace");
    temp.write_project_file("apps/web", "package.json", b"{}")
        .expect("package json");
    let git = temp.create_git_repo("apps/web").expect("git repo");
    std::fs::write(git.join("refs").join("heads").join("main"), b"abc123\n").expect("branch ref");
    std::fs::create_dir_all(git.join("objects").join("ab")).expect("object dir");
    std::fs::write(git.join("objects").join("ab").join("cdef"), b"loose-object")
        .expect("loose object");
    std::fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
    std::fs::write(
        git.join("objects").join("pack").join("pack-main-001.pack"),
        b"pack-bytes",
    )
    .expect("pack file");
    let detector = temp.mutation_detector().expect("mutation detector");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    detector.assert_unchanged().expect("scan should not mutate");
    for path in [
        "apps/web/.git/refs/heads/main",
        "apps/web/.git/objects/ab/cdef",
        "apps/web/.git/objects/pack/pack-main-001.pack",
    ] {
        assert!(report.paths.iter().any(|observed| {
            observed.path == path
                && serde_json::to_value(observed.policy.mode).unwrap() == "encrypted-sync"
                && serde_json::to_value(observed.policy.classification).unwrap() == "workspace-sync"
        }));
    }
}

#[test]
fn scan_records_worktree_gitlink_file_as_encrypted_workspace_bytes() {
    let temp =
        crate::workspace::TempWorkspace::new("scan-worktree-gitlink").expect("temp workspace");
    std::fs::create_dir_all(temp.root().join("repo-wt")).expect("worktree dir");
    std::fs::write(
        temp.root().join("repo-wt").join(".git"),
        b"gitdir: /tmp/root/repo/.git/worktrees/repo-wt\n",
    )
    .expect("gitlink");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert!(report.paths.iter().any(|observed| {
        observed.path == "repo-wt/.git"
            && !observed.is_dir
            && serde_json::to_value(observed.policy.mode).unwrap() == "encrypted-sync"
            && serde_json::to_value(observed.policy.classification).unwrap() == "workspace-sync"
    }));
}

#[cfg(unix)]
#[test]
fn scan_records_dangling_symlink_without_following_it() {
    let temp = crate::workspace::TempWorkspace::new("scan-symlink").expect("temp workspace");
    temp.write_file("package.json", b"{}")
        .expect("package json");
    temp.create_symlink("", "missing-link", "does-not-exist")
        .expect("dangling symlink");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert!(report.paths.iter().any(|path| {
        path.path == "missing-link"
            && !path.is_dir
            && serde_json::to_value(path.policy.mode).unwrap() == "workspace-sync"
    }));
}

#[test]
fn scanner_does_not_restore_explicitly_included_dependency_paths() {
    let temp = crate::workspace::TempWorkspace::new("scan-include").expect("temp workspace");
    temp.write_file(".bowlineignore", b"!node_modules/kept.js\n")
        .expect("ignore");
    temp.write_file("package.json", b"{}")
        .expect("package json");
    temp.write_file("node_modules/kept.js", b"module.exports = {}\n")
        .expect("included dependency");
    temp.write_file("node_modules/skipped.js", b"module.exports = {}\n")
        .expect("skipped dependency");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert!(
        !report
            .paths
            .iter()
            .any(|path| path.path == "node_modules/kept.js")
    );
    assert!(
        !report
            .paths
            .iter()
            .any(|path| path.path == "node_modules/skipped.js")
    );
}

#[test]
fn scanner_keeps_work_view_namespace_out_of_canonical_workspace_state() {
    let temp = crate::workspace::TempWorkspace::new("scan-work-namespace").expect("temp workspace");
    temp.write_file("package.json", b"{}")
        .expect("package json");
    temp.write_file(".work/app/feature/src/auth.ts", b"work view edit\n")
        .expect("work file");

    let report = scan_workspace(temp.root()).expect("scan succeeds");

    assert!(report.paths.iter().any(|path| {
        path.path == ".work" && serde_json::to_value(path.policy.mode).unwrap() == "local-only"
    }));
    assert!(
        !report
            .paths
            .iter()
            .any(|path| path.path == ".work/app/feature/src/auth.ts"),
        ".work contents must not become canonical workspace paths"
    );
}

#[test]
fn project_ids_do_not_collide_for_punctuation_variants() {
    let temp = crate::workspace::TempWorkspace::new("scan-project-ids").expect("temp workspace");
    temp.write_project_file("apps/web-api", "package.json", b"{}")
        .expect("first project");
    temp.write_project_file("apps/web_api", "package.json", b"{}")
        .expect("second project");

    let report = scan_workspace(temp.root()).expect("scan succeeds");
    let ids = report
        .projects
        .iter()
        .map(|project| project.id.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(report.projects.len(), 2);
    assert_eq!(ids.len(), 2);
}

#[test]
fn root_shallow_records_direct_children_and_root_project_without_nesting() {
    let temp =
        crate::workspace::TempWorkspace::new("scan-root-shallow-children").expect("workspace");
    temp.write_file("package.json", b"{}")
        .expect("root identity");
    temp.write_file("README.md", b"top\n").expect("root file");
    temp.write_file("apps/web/src/deep.rs", b"fn deep() {}\n")
        .expect("deep file");

    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan");
    let paths = report
        .paths
        .iter()
        .map(|path| path.path.as_str())
        .collect::<Vec<_>>();

    assert!(paths.contains(&"package.json"));
    assert!(paths.contains(&"README.md"));
    assert!(
        report
            .paths
            .iter()
            .any(|path| path.path == "apps" && path.is_dir),
        "the root directory child is recorded but not descended"
    );
    assert!(
        !report.paths.iter().any(|path| path.path.contains('/')),
        "root-shallow scan must not contain any nested path"
    );
    assert!(
        report
            .projects
            .iter()
            .any(|project| project.path.is_empty()),
        "root package.json still classifies the root project"
    );
}

#[test]
fn root_shallow_classifies_root_project_from_cargo_manifest() {
    let temp = crate::workspace::TempWorkspace::new("scan-root-shallow-cargo").expect("workspace");
    temp.write_file("Cargo.toml", b"[package]\nname = \"root\"\n")
        .expect("cargo manifest");

    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan");

    assert!(!report.projects.is_empty());
    assert!(
        report
            .projects
            .iter()
            .any(|project| project.path.is_empty())
    );
}

#[test]
fn root_shallow_does_not_descend_into_subtrees() {
    let temp = crate::workspace::TempWorkspace::new("scan-root-shallow-cost").expect("workspace");
    temp.write_file("README.md", b"top\n").expect("root file");
    // A deep subtree whose descent would be observable if the scan recursed.
    temp.write_file("service/api/src/deep/module.rs", b"fn deep() {}\n")
        .expect("deep module");
    temp.write_file("service/api/pkg/more/other.rs", b"fn other() {}\n")
        .expect("deep other");
    let direct_children = std::fs::read_dir(temp.root()).expect("root read").count() as u64;

    crate::fs_access::install(temp.root());
    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan");
    let counts = crate::fs_access::take();

    assert_eq!(
        counts.root_read_dir_count, 1,
        "exactly one read_dir of the root"
    );
    assert_eq!(
        counts.subdir_read_dir_count, 0,
        "no subdirectory read_dir during a root-shallow tick"
    );
    assert_eq!(
        counts.metadata_count, direct_children,
        "metadata calls bounded by the root's direct children"
    );
    // Output absence alone is insufficient — the zero subdir read proves the
    // deep file was never observed, not merely filtered after the work.
    assert!(
        !report
            .paths
            .iter()
            .any(|path| path.path == "service/api/src/deep/module.rs")
    );
}

#[test]
fn scoped_scan_does_not_walk_unrelated_subtrees_for_policy() {
    // Regression: a `DirtySubtrees` tick must load policy bounded to its dirty
    // roots, not restat all of `~/Code`. Before load_scoped, scan_workspace_scoped
    // called UserPolicy::load, which recursively read every directory (including
    // the unrelated deep `other/*` tree) hunting for `.bowlineignore`.
    let temp = crate::workspace::TempWorkspace::new("scan-scoped-policy-cost").expect("workspace");
    temp.write_file(".bowlineignore", b"notes.txt\n")
        .expect("root policy");
    temp.write_file("service/api/mod.rs", b"fn api() {}\n")
        .expect("dirty file");
    // A deep, unrelated subtree whose per-directory policy read_dir would show up
    // if scoped policy loading walked the whole workspace.
    temp.write_file("other/l1/l2/l3/l4/mod.rs", b"fn deep() {}\n")
        .expect("deep unrelated");

    let roots: BTreeSet<String> = std::iter::once("service/api".to_string()).collect();
    crate::fs_access::install(temp.root());
    let report = scan_workspace_scoped(temp.root(), &roots).expect("scoped scan");
    let counts = crate::fs_access::take();

    // Only the dirty subtree (and its policy load) may be read; the 5-level
    // `other/*` tree must contribute zero directory reads.
    assert!(
        counts.subdir_read_dir_count <= 4,
        "scoped scan walked unrelated subtrees ({} subdir reads)",
        counts.subdir_read_dir_count
    );
    assert!(
        !report
            .paths
            .iter()
            .any(|path| path.path.starts_with("other/")),
        "scoped scan observed an unrelated subtree"
    );
}

#[test]
fn root_shallow_reads_only_root_policy_inputs() {
    let temp = crate::workspace::TempWorkspace::new("scan-root-shallow-policy").expect("workspace");
    temp.write_file(".bowlineignore", b"notes.txt\n")
        .expect("root policy");
    temp.write_file("notes.txt", b"local notes\n")
        .expect("ignored file");
    temp.write_file("README.md", b"top\n").expect("root file");
    // A nested policy tree that a recursive policy load would walk into.
    temp.write_file("deep/nested/.bowlineignore", b"secret/**\n")
        .expect("nested policy");
    temp.write_file("deep/nested/more/file.rs", b"fn f() {}\n")
        .expect("deep file");

    crate::fs_access::install(temp.root());
    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan");
    let counts = crate::fs_access::take();

    assert_eq!(
        counts.subdir_read_dir_count, 0,
        "no deep policy-discovery read_dir"
    );
    let notes = report
        .paths
        .iter()
        .find(|path| path.path == "notes.txt")
        .expect("ignored file observed");
    assert_eq!(
        serde_json::to_value(notes.policy.classification).unwrap(),
        "local-only",
        "root-level .bowlineignore was applied"
    );
}

#[test]
fn root_shallow_defers_expensive_git_untracked_health() {
    let temp = crate::workspace::TempWorkspace::new("scan-root-shallow-git").expect("workspace");
    temp.write_file("package.json", b"{}")
        .expect("root identity");
    let git = temp.root().join(".git");
    std::fs::create_dir_all(git.join("refs/heads")).expect("git dir");
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
    std::fs::write(git.join("config"), b"[core]\n").expect("config");
    // A large deep untracked tree that a full scan would walk to count.
    temp.write_file("src/deep/a.rs", b"fn a() {}\n")
        .expect("untracked a");
    temp.write_file("src/deep/b.rs", b"fn b() {}\n")
        .expect("untracked b");
    temp.write_file("src/deep/nested/c.rs", b"fn c() {}\n")
        .expect("untracked c");

    crate::fs_access::install(temp.root());
    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan");
    let counts = crate::fs_access::take();

    assert_eq!(
        counts.subdir_read_dir_count, 0,
        "no recursive Git untracked walk during a root-shallow tick"
    );
    let root_project = report
        .projects
        .iter()
        .find(|project| project.path.is_empty())
        .expect("root project observed");
    assert!(
        root_project.has_git_repo,
        "cheap classification still updates"
    );
    assert!(
        root_project.health_refresh_needed,
        "expensive health is marked for refresh"
    );
    assert_eq!(
        root_project.untracked_file_count, 0,
        "deferred untracked count is a placeholder, not a deep walk result"
    );
}

#[test]
fn root_shallow_handles_empty_workspace_root() {
    let temp = crate::workspace::TempWorkspace::new("scan-root-shallow-empty").expect("workspace");

    crate::fs_access::install(temp.root());
    let report = scan_workspace_root_shallow(temp.root()).expect("root-shallow scan succeeds");
    let counts = crate::fs_access::take();

    assert!(report.paths.is_empty());
    assert!(report.projects.is_empty());
    assert_eq!(counts.root_read_dir_count, 1);
    assert_eq!(counts.subdir_read_dir_count, 0);
}

#[test]
fn merge_prefers_scoped_observation_for_shared_root_directory() {
    // Both passes observe the dirty root's own directory entry `src`; the scoped
    // observation (byte_len None) must win over the shallow one (byte_len marker).
    let scoped = report_with(vec![
        dir_observation("src", None),
        path_observation("src/app.rs"),
    ]);
    let shallow = report_with(vec![
        dir_observation("src", Some(999)),
        path_observation("README.md"),
    ]);
    let roots = BTreeSet::from(["src".to_string()]);

    let merged = merge_scoped_and_shallow_reports(scoped, shallow, &roots);

    let src = merged
        .paths
        .iter()
        .find(|observed| observed.path == "src")
        .expect("src entry present");
    assert_eq!(src.byte_len, None, "scoped-owned src observation must win");
    let paths = merged
        .paths
        .iter()
        .map(|observed| observed.path.as_str())
        .collect::<BTreeSet<_>>();
    assert!(paths.contains("src/app.rs"), "scoped deep path retained");
    assert!(paths.contains("README.md"), "shallow root path retained");
    // Exactly one `src` entry survives the merge.
    assert_eq!(merged.paths.iter().filter(|o| o.path == "src").count(), 1);
}

#[test]
fn merge_combines_scoped_deep_and_shallow_root_entries() {
    let scoped = report_with(vec![path_observation("src/app.rs")]);
    let shallow = report_with(vec![
        path_observation("README.md"),
        dir_observation("logs", None),
    ]);
    let roots = BTreeSet::from(["src".to_string()]);

    let merged = merge_scoped_and_shallow_reports(scoped, shallow, &roots);
    let paths = merged
        .paths
        .iter()
        .map(|observed| observed.path.as_str())
        .collect::<BTreeSet<_>>();

    assert!(paths.contains("src/app.rs"));
    assert!(paths.contains("README.md"));
    assert!(paths.contains("logs"));
}

fn report_with(paths: Vec<PathObservation>) -> ScanReport {
    ScanReport {
        root: std::path::PathBuf::from("/tmp/bowline-merge-report"),
        projects: Vec::new(),
        paths,
        summary: ObservedWorkspaceSummary::default(),
    }
}

fn dir_observation(path: &str, byte_len: Option<u64>) -> PathObservation {
    let mut observation = path_observation(path);
    observation.is_dir = true;
    observation.byte_len = byte_len;
    observation
}

fn path_observation(path: &str) -> PathObservation {
    PathObservation {
        path: path.to_string(),
        project_id: None,
        is_dir: false,
        is_symlink: false,
        byte_len: Some(8),
        stat: None,
        executability: FileExecutability::Regular,
        policy: classify_path_with_builtin_policy(path),
    }
}

fn minimal_git_index_header(version: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes
}

#[cfg(unix)]
fn executability_for_path(report: &ScanReport, path: &str) -> FileExecutability {
    report
        .paths
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.executability)
        .expect("path observation")
}
