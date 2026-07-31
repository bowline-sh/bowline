use std::fs;

use super::*;
use crate::cli::parse_args;
use bowline_core::commands::DiffUnavailable;
use bowline_local::sync::manifest_engine::CONFLICT_ASIDE_MARKER;

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("bowline-resolve-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("workspace root");
        Self { root }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, contents).expect("write file");
    }

    fn symlink(&self, relative: &str, target: &Path) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        std::os::unix::fs::symlink(target, path).expect("symlink");
    }
}

/// A file planted OUTSIDE the workspace root, where a `../` traversal from the
/// root reaches it. Removed with the test so a failure leaves nothing behind.
struct OutsideFile {
    name: String,
    path: PathBuf,
}

impl OutsideFile {
    fn new(name: &str, contents: &str) -> Self {
        let path = std::env::temp_dir().join(name);
        fs::write(&path, contents).expect("outside file");
        Self {
            name: name.to_string(),
            path,
        }
    }
}

impl Drop for OutsideFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn aside_of(path: &str) -> String {
    format!("{path}{CONFLICT_ASIDE_MARKER}deadbeef")
}

fn resolve_args(workspace: &Workspace, aside: &str, action: ConflictAction) -> ResolveArgs {
    ResolveArgs {
        root: workspace.root.display().to_string(),
        aside_path: aside.to_string(),
        action,
    }
}

#[test]
fn keep_local_leaves_the_file_and_removes_the_incoming_version() {
    let workspace = Workspace::new("keep-local");
    workspace.write("src/auth.ts", "local");
    workspace.write(&aside_of("src/auth.ts"), "remote");

    print_resolve(
        resolve_args(
            &workspace,
            &aside_of("src/auth.ts"),
            ConflictAction::KeepLocal,
        ),
        true,
    );

    assert_eq!(
        fs::read_to_string(workspace.root.join("src/auth.ts")).expect("file"),
        "local",
    );
    assert!(!workspace.root.join(aside_of("src/auth.ts")).exists());
}

#[test]
fn take_remote_replaces_the_file_with_the_incoming_version() {
    let workspace = Workspace::new("take-remote");
    workspace.write("src/auth.ts", "local");
    workspace.write(&aside_of("src/auth.ts"), "remote");

    print_resolve(
        resolve_args(
            &workspace,
            &aside_of("src/auth.ts"),
            ConflictAction::TakeRemote,
        ),
        true,
    );

    assert_eq!(
        fs::read_to_string(workspace.root.join("src/auth.ts")).expect("file"),
        "remote",
    );
    assert!(!workspace.root.join(aside_of("src/auth.ts")).exists());
}

#[test]
fn timeline_workspace_reports_lookup_errors_but_not_missing_roots() {
    let mut reported = Vec::new();
    let failed = timeline_workspace(
        Result::<Option<&str>, _>::Err("metadata lookup failed"),
        |error| reported.push(*error),
    );
    let missing = timeline_workspace(Result::<Option<&str>, &str>::Ok(None), |_| {
        panic!("a missing accepted root is not a metadata failure");
    });

    assert_eq!(failed, None);
    assert_eq!(missing, None);
    assert_eq!(reported, ["metadata lookup failed"]);
}

#[test]
fn diff_changes_nothing_on_disk() {
    let workspace = Workspace::new("diff");
    workspace.write("src/auth.ts", "local\n");
    workspace.write(&aside_of("src/auth.ts"), "remote\n");

    print_resolve(
        resolve_args(&workspace, &aside_of("src/auth.ts"), ConflictAction::Diff),
        true,
    );

    assert_eq!(
        fs::read_to_string(workspace.root.join("src/auth.ts")).expect("file"),
        "local\n",
    );
    assert!(workspace.root.join(aside_of("src/auth.ts")).exists());
}

#[test]
fn resolve_refuses_a_path_that_climbs_out_of_the_workspace() {
    let workspace = Workspace::new("traversal");
    let outside = OutsideFile::new(
        &aside_of(&format!("bowline-cli-outside-{}.env", std::process::id())),
        "STRIPE_SECRET_KEY=real",
    );

    print_resolve(
        resolve_args(
            &workspace,
            &format!("../{}", outside.name),
            ConflictAction::KeepLocal,
        ),
        true,
    );

    assert!(
        outside.path.exists(),
        "resolve deleted a file outside the workspace",
    );
}

#[test]
fn diff_reports_an_aside_shaped_symlink_instead_of_printing_its_target() {
    // The aside name is the only thing the scan checks, so a symlink can wear
    // one. `--diff` prints to a terminal, so following it would disclose any
    // file the user can read.
    const SECRET: &str = "BEGIN OPENSSH PRIVATE KEY";
    let workspace = Workspace::new("diff-symlink");
    let secret = OutsideFile::new(
        &format!("bowline-cli-secret-{}.pem", std::process::id()),
        SECRET,
    );
    workspace.write("notes.md", "local\n");
    workspace.symlink(&aside_of("notes.md"), &secret.path);

    let conflict = conflict_at(&workspace.root, &WorkspacePath::new(aside_of("notes.md")))
        .expect("the aside is reachable by name");
    let printed = match unified_diff(&workspace.root, &conflict).expect("a workspace path") {
        DiffOutcome::Unified(text) => text,
        DiffOutcome::Unavailable(reason) => reason.message().to_string(),
    };

    assert!(!printed.contains(SECRET), "{printed}");
    assert_eq!(printed, DiffUnavailable::Symlink.message());
}

#[test]
fn resolve_requires_exactly_one_verb() {
    // Silently defaulting would pick a side of the user's own work.
    let missing = parse_args(["resolve", "some.txt.bowline-conflict.abc"])
        .command
        .expect_err("no verb is a usage error");
    assert!(
        matches!(&missing, ParseError::Command(error) if error.code == "missing_required_option"),
        "{missing:?}",
    );

    let ambiguous = parse_args([
        "resolve",
        "some.txt.bowline-conflict.abc",
        "--keep-local",
        "--take-remote",
    ])
    .command
    .expect_err("two verbs is a usage error");
    assert!(
        matches!(ambiguous, ParseError::Usage { .. }),
        "{ambiguous:?}"
    );
}

/// The preview must accept every path the irreversible verbs accept. It located
/// conflicts through the workspace scan, which prunes subtrees and stops at an
/// entry budget, so `--diff` refused precisely where `--take-remote` succeeded.
#[test]
fn the_preview_reads_exactly_where_the_mutating_verbs_write() {
    let workspace = Workspace::new("pruned-preview");
    workspace.write("node_modules/ui/index.ts", "local\n");
    workspace.write(&aside_of("node_modules/ui/index.ts"), "remote\n");
    let aside = WorkspacePath::new(aside_of("node_modules/ui/index.ts"));

    let (conflict, outcome) =
        diff_only(&workspace.root, &aside).expect("the preview reaches this aside");
    assert_eq!(conflict.origin.as_str(), "node_modules/ui/index.ts");
    assert!(
        matches!(outcome, Some(DiffOutcome::Unified(ref text)) if text.contains("-local")),
        "expected a diff of both sides",
    );

    apply(&workspace.root, &aside, ConflictResolution::KeepLocal)
        .expect("the mutating verb acts on the same aside");
}

fn selection_of(workspace: &Workspace, project: Option<&str>) -> WorkspaceSelection {
    WorkspaceSelection {
        root: workspace.root.display().to_string(),
        project: project.map(str::to_string),
    }
}

#[test]
fn the_workspace_root_as_project_still_lists_its_conflicts() {
    // `--project <root>` is relative-empty, and an empty prefix matched no aside
    // at all: the command reported a clean workspace and exited 0.
    let workspace = Workspace::new("project-root");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");
    let selection = selection_of(&workspace, Some(&workspace.root.display().to_string()));

    let scope = project_scope(&workspace.root, &selection).expect("the root is in the workspace");

    assert_eq!(scope, None);
    let conflicts = list_conflicts(&workspace.root).expect("scan");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(
        conflicts
            .iter()
            .all(|conflict| in_project_scope(conflict, scope.as_ref()))
    );
}

#[test]
fn a_project_below_the_root_narrows_to_its_own_subtree() {
    let workspace = Workspace::new("project-subtree");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");
    workspace.write("other/api/src/auth.ts", "local");
    workspace.write(&aside_of("other/api/src/auth.ts"), "remote");

    for project in [
        "acme/web",
        &workspace.root.join("acme/web").display().to_string(),
    ] {
        let selection = selection_of(&workspace, Some(project));
        let scope = project_scope(&workspace.root, &selection).expect("inside the workspace");

        let kept = list_conflicts(&workspace.root)
            .expect("scan")
            .into_iter()
            .filter(|conflict| in_project_scope(conflict, scope.as_ref()))
            .map(|conflict| conflict.aside.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(kept, vec![aside_of("acme/web/src/auth.ts")], "{project}");
    }
}

/// A workspace path may legally contain the literal flag text, so deriving the
/// `--diff` and `--keep-local` suggestions by rewriting the finished command
/// edited the path instead of the verb and pointed an agent at an aside that
/// does not exist.
#[test]
fn a_suggested_variant_rewrites_the_verb_and_never_the_path() {
    let rendered = "bowline resolve 'notes--take-remote.md.bowline-conflict.abcd1234' --root '~/Code' --take-remote";

    let diff = super::resolve_command_variant(rendered, "--diff");

    assert!(
        diff.contains("notes--take-remote.md.bowline-conflict.abcd1234"),
        "the path must survive the rewrite: {diff}"
    );
    assert!(diff.ends_with(" --diff"), "{diff}");
    assert!(!diff.ends_with("--take-remote"), "{diff}");
}

#[test]
fn a_project_naming_the_conflicted_file_itself_lists_its_conflict() {
    // `--project` is documented as a "project or path", so naming the affected
    // file is a natural way to ask about it. The aside is not *under* that path,
    // it sits beside it, and the subtree rule alone reported a clean workspace
    // for the one file the user asked about.
    let workspace = Workspace::new("project-file");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");
    workspace.write("acme/web/src/other.ts", "local");
    workspace.write(&aside_of("acme/web/src/other.ts"), "remote");

    let selection = selection_of(&workspace, Some("acme/web/src/auth.ts"));
    let scope = project_scope(&workspace.root, &selection).expect("inside the workspace");

    let kept = list_conflicts(&workspace.root)
        .expect("scan")
        .into_iter()
        .filter(|conflict| in_project_scope(conflict, scope.as_ref()))
        .map(|conflict| conflict.aside.as_str().to_string())
        .collect::<Vec<_>>();

    assert_eq!(kept, vec![aside_of("acme/web/src/auth.ts")]);
}

#[test]
fn a_project_the_workspace_has_no_files_under_yet_still_narrows() {
    // A workspace-relative `--project` names a place in this workspace whether
    // or not anything is there; only a path that leaves the root is a usage
    // error.
    let workspace = Workspace::new("project-absent");

    let scope = project_scope(&workspace.root, &selection_of(&workspace, Some("acme/web")))
        .expect("a workspace-relative project stays in the workspace");

    assert_eq!(scope.as_ref().map(ProjectScope::as_str), Some("acme/web"));
}

#[test]
fn a_project_outside_the_workspace_is_named_rather_than_reported_as_clean() {
    let workspace = Workspace::new("project-outside");
    let outside = std::env::temp_dir().display().to_string();

    let error = project_scope(&workspace.root, &selection_of(&workspace, Some(&outside)))
        .expect_err("a project outside the root cannot narrow this workspace");

    assert_eq!(error.as_str(), outside);
}
