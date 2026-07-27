use std::{fs, path::Path};

use super::*;
use crate::sync::manifest_engine::CONFLICT_ASIDE_MARKER;

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("bowline-conflicts-{name}-{}", std::process::id()));
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

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.root.join(relative)).expect("read file")
    }

    fn exists(&self, relative: &str) -> bool {
        self.root.join(relative).exists()
    }

    fn symlink(&self, relative: &str, target: &Path) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        std::os::unix::fs::symlink(target, path).expect("symlink");
    }
}

/// A file planted OUTSIDE the workspace root, at the path a `../` traversal
/// from the root would reach. Cleaned up with the test so a failure cannot leave
/// a stray file behind in the temp directory.
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

    fn exists(&self) -> bool {
        self.path.exists()
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

#[test]
fn a_conflict_aside_is_listed_with_the_path_it_sits_beside() {
    let workspace = Workspace::new("list");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");

    let conflicts = list_conflicts(&workspace.root).expect("scan");

    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].origin.as_str(), "acme/web/src/auth.ts");
    assert_eq!(
        conflicts[0].aside.as_str(),
        aside_of("acme/web/src/auth.ts")
    );
    assert!(!conflicts[0].origin_missing);
}

#[test]
fn a_workspace_without_asides_reports_no_conflicts() {
    let workspace = Workspace::new("clean");
    workspace.write("acme/web/src/auth.ts", "local");

    assert!(list_conflicts(&workspace.root).expect("scan").is_empty());
}

/// Restores a directory's permissions when the test ends, so a failed assertion
/// cannot leave an unremovable temp directory behind.
struct Unreadable {
    path: PathBuf,
}

impl Unreadable {
    fn new(path: PathBuf) -> Self {
        chmod(&path, 0o000);
        Self { path }
    }
}

impl Drop for Unreadable {
    fn drop(&mut self) {
        chmod(&self.path, 0o700);
    }
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A mistyped `--root`, a renamed workspace, or an external drive that never
/// mounted. Answering "no conflicts, exit 0" would tell the user their workspace
/// is reconciled when it is not even there.
#[test]
fn a_missing_root_is_a_named_fault_rather_than_an_empty_scan() {
    let workspace = Workspace::new("root-missing");
    let missing = workspace.root.join("not-mounted");

    let error = list_conflicts(&missing).expect_err("a missing root cannot be scanned");

    assert!(
        matches!(&error, ConflictError::Root { fault, .. } if *fault == RootFault::Missing),
        "{error:?}"
    );
    assert_eq!(error.tag(), "root-missing");
}

#[test]
fn a_root_that_is_not_a_directory_is_a_named_fault() {
    let workspace = Workspace::new("root-not-a-directory");
    workspace.write("in-the-way", "not a workspace");

    let error = list_conflicts(&workspace.root.join("in-the-way"))
        .expect_err("a file is not a workspace root");

    assert!(
        matches!(&error, ConflictError::Root { fault, .. } if *fault == RootFault::NotADirectory),
        "{error:?}"
    );
}

/// A permission problem on the root reads as an empty directory to `read_dir`,
/// which is the same silent-failure shape as a missing root.
#[test]
fn an_unreadable_root_is_a_named_fault_rather_than_an_empty_scan() {
    let workspace = Workspace::new("root-unreadable");
    workspace.write(&aside_of("notes.md"), "remote");
    let _unreadable = Unreadable::new(workspace.root.clone());

    let error = list_conflicts(&workspace.root).expect_err("an unreadable root cannot be scanned");

    assert!(
        matches!(&error, ConflictError::Root { fault, .. } if *fault == RootFault::Unreadable),
        "{error:?}"
    );
    assert_eq!(error.tag(), "root-unreadable");
}

/// One locked-down subtree is a fact about that subtree. The conflicts the user
/// can actually act on still have to be listed.
#[test]
fn an_unreadable_subdirectory_still_yields_every_visible_conflict() {
    let workspace = Workspace::new("subtree-unreadable");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");
    workspace.write("locked/secret.txt", "local");
    let _unreadable = Unreadable::new(workspace.root.join("locked"));

    let conflicts = list_conflicts(&workspace.root).expect("a locked subtree never fails the scan");

    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(
        conflicts[0].aside.as_str(),
        aside_of("acme/web/src/auth.ts")
    );
}

#[test]
fn an_aside_whose_file_was_deleted_is_still_reported() {
    // The origin can be gone (deleted locally after the conflict landed). The
    // aside is then the only surviving copy, so it must stay visible.
    let workspace = Workspace::new("orphan");
    workspace.write(&aside_of("notes.md"), "remote");

    let conflicts = list_conflicts(&workspace.root).expect("scan");

    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].origin_missing);
}

#[test]
fn the_scan_never_descends_into_git_state() {
    // The engine refuses to write asides under `.git/**`; a name that looks like
    // one there is git's own business and must not be reported or touched.
    let workspace = Workspace::new("git");
    workspace.write("acme/web/.git/HEAD", "ref: refs/heads/main");
    workspace.write(&aside_of("acme/web/.git/HEAD"), "ref: refs/heads/other");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");

    let conflicts = list_conflicts(&workspace.root).expect("scan");

    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.aside.as_str())
            .collect::<Vec<_>>(),
        vec![aside_of("acme/web/src/auth.ts")],
    );
}

/// Build a directory chain whose ABSOLUTE path is longer than the platform's
/// `PATH_MAX`, then put `leaf` inside it. Every step is one `mkdirat`/`openat`
/// against the descriptor above it, because the test itself cannot hand the
/// kernel a path this long either.
fn create_beyond_path_max(root: &Path, components: &[String], leaf: &str, contents: &[u8]) {
    use std::io::Write;

    use rustix::fs::{Mode, OFlags};

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let directory_mode = Mode::from_bits_truncate(0o700);
    let mut directory =
        rustix::fs::open(root, directory_flags, Mode::empty()).expect("open the workspace root");
    for component in components {
        rustix::fs::mkdirat(&directory, component.as_str(), directory_mode)
            .expect("create a level");
        directory = rustix::fs::openat(
            &directory,
            component.as_str(),
            directory_flags,
            Mode::empty(),
        )
        .expect("descend a level");
    }
    let file = rustix::fs::openat(
        &directory,
        leaf,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .expect("create the aside");
    fs::File::from(file)
        .write_all(contents)
        .expect("write the aside");
}

/// The longest workspace-relative directory path that still leaves room for
/// `leaf`, as components no longer than a filesystem name limit.
fn longest_path_components(leaf: &str) -> Vec<String> {
    const COMPONENT_LEN: usize = 250;
    let budget = MAX_WORKSPACE_PATH_LEN as usize - leaf.len() - 1;
    let mut components = Vec::new();
    let mut used = 0_usize;
    while used + COMPONENT_LEN < budget {
        components.push("d".repeat(COMPONENT_LEN));
        used += COMPONENT_LEN + 1;
    }
    // One short component spends what is left, so the relative path sits at the
    // engine's own ceiling.
    if used + 1 < budget {
        components.push("d".repeat(budget - used - 1));
    }
    components
}

/// A root long enough that a relative path at the engine's ceiling still clears
/// the kernel's `PATH_MAX` when joined to it.
///
/// The relative path alone cannot guarantee that: it is capped at
/// `MAX_WORKSPACE_PATH_LEN`, which is exactly `PATH_MAX` on Linux, so whether the
/// join overflows depends on how long the temp directory happens to be. That is
/// ~60 characters under `/var/folders/...` on macOS and ~20 under `/tmp` on a CI
/// runner, so the control assertion held locally and failed in CI — the test
/// correctly refused to prove nothing. Padding the root removes the dependency
/// on where the suite happens to run.
fn padded_root(base: &Path) -> PathBuf {
    const PATH_MAX: usize = 4096;
    let mut root = base.to_path_buf();
    while root.as_os_str().len() + MAX_WORKSPACE_PATH_LEN as usize <= PATH_MAX + 64 {
        root = root.join("p".repeat(64));
    }
    root
}

/// The scan descends through held descriptors, one `openat` per name, and never
/// rebuilds `root.join(relative)` for the kernel to resolve again. This pins the
/// mechanism where its absence is visible without a race: a path the kernel will
/// not resolve as a whole is still walked, one component at a time, exactly as a
/// directory swapped for a symlink mid-walk is still walked through the
/// descriptor that was opened before the swap.
#[test]
fn the_scan_descends_by_descriptor_rather_than_by_rebuilt_path() {
    let base = Workspace::new("deep");
    // The root is padded so the join clears PATH_MAX wherever the suite runs.
    let root = padded_root(&base.root);
    fs::create_dir_all(&root).expect("padded workspace root");
    let workspace = Workspace { root };
    let aside = aside_of("notes.md");
    let components = longest_path_components(&aside);
    create_beyond_path_max(&workspace.root, &components, &aside, b"remote");
    let relative_directory = components.join("/");
    let relative_aside = format!("{relative_directory}/{aside}");

    // Control: the walk this scan must never go back to cannot even reach the
    // directory, so a listing that finds the aside anyway can only have got
    // there by descriptor.
    assert!(
        fs::read_dir(workspace.root.join(&relative_directory)).is_err(),
        "the path must actually be unresolvable, or this test proves nothing",
    );

    let conflicts = list_conflicts(&workspace.root).expect("scan");

    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.aside.as_str())
            .collect::<Vec<_>>(),
        vec![relative_aside.as_str()],
    );
    // Answered by `fstatat` against the aside's own directory descriptor. A
    // path-based probe of the sibling fails `ENAMETOOLONG` here, which is not
    // absence, so it would report the origin as still present.
    assert!(conflicts[0].origin_missing);
}

#[test]
fn keeping_local_drops_the_incoming_version_and_leaves_the_file_alone() {
    let workspace = Workspace::new("keep-local");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");

    let resolved = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        ConflictResolution::KeepLocal,
    )
    .expect("resolve");

    assert_eq!(resolved.resolution, ConflictResolution::KeepLocal);
    assert_eq!(workspace.read("acme/web/src/auth.ts"), "local");
    assert!(!workspace.exists(&aside_of("acme/web/src/auth.ts")));
    assert!(list_conflicts(&workspace.root).expect("scan").is_empty());
}

#[test]
fn taking_remote_replaces_the_file_with_the_incoming_version() {
    let workspace = Workspace::new("take-remote");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");

    let resolved = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        ConflictResolution::TakeRemote,
    )
    .expect("resolve");

    assert_eq!(resolved.resolution, ConflictResolution::TakeRemote);
    assert_eq!(workspace.read("acme/web/src/auth.ts"), "remote");
    assert!(!workspace.exists(&aside_of("acme/web/src/auth.ts")));
    assert!(list_conflicts(&workspace.root).expect("scan").is_empty());
}

#[test]
fn taking_remote_restores_a_file_the_local_side_deleted() {
    let workspace = Workspace::new("take-remote-orphan");
    workspace.write(&aside_of("acme/web/src/auth.ts"), "remote");

    resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        ConflictResolution::TakeRemote,
    )
    .expect("resolve");

    assert_eq!(workspace.read("acme/web/src/auth.ts"), "remote");
}

#[test]
fn a_path_that_is_not_an_aside_is_refused_by_name() {
    let workspace = Workspace::new("not-an-aside");
    workspace.write("acme/web/src/auth.ts", "local");

    let error = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new("acme/web/src/auth.ts"),
        ConflictResolution::KeepLocal,
    )
    .expect_err("the origin path is not resolvable");

    assert_eq!(error.tag(), "not_a_conflict_aside");
}

#[test]
fn a_path_that_climbs_out_of_the_workspace_never_reaches_the_filesystem() {
    // `root.join("../x")` names a real file the user owns. Resolving it would
    // delete (keep-local) or rename over (take-remote) a file the workspace does
    // not contain, so the path is refused before anything is stat'd.
    let workspace = Workspace::new("traversal");
    let outside = OutsideFile::new(
        &aside_of(&format!("bowline-outside-{}.env", std::process::id())),
        "AWS_SECRET_ACCESS_KEY=real",
    );
    let escape = format!("../{}", outside.name);

    for resolution in [
        ConflictResolution::KeepLocal,
        ConflictResolution::TakeRemote,
    ] {
        let error = resolve_conflict(&workspace.root, &WorkspacePath::new(&escape), resolution)
            .expect_err("a traversal path is not resolvable");
        assert_eq!(error.tag(), "conflict_path_refused", "{error}");
    }
    assert!(outside.exists(), "the file outside the workspace survived");
}

#[test]
fn an_absolute_path_is_refused_rather_than_reinterpreted() {
    // Trimming the leading slash would turn `/etc/hosts...` into the workspace's
    // own `etc/hosts...` — a different file, silently.
    let workspace = Workspace::new("absolute");
    let absolute = format!("/{}", aside_of("etc/hosts"));

    let error = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(absolute),
        ConflictResolution::KeepLocal,
    )
    .expect_err("an absolute path is not resolvable");

    assert_eq!(error.tag(), "conflict_path_refused");
}

#[test]
fn private_engine_state_is_neither_listed_nor_resolvable() {
    let workspace = Workspace::new("engine-state");
    workspace.write(&aside_of(".bowline/state"), "remote");

    let error = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of(".bowline/state")),
        ConflictResolution::KeepLocal,
    )
    .expect_err("engine state is not a user conflict");

    assert_eq!(error.tag(), "conflict_path_refused");
    assert!(workspace.exists(&aside_of(".bowline/state")));
    // The listing would otherwise print a resolve command that always fails.
    assert!(list_conflicts(&workspace.root).expect("scan").is_empty());
}

#[test]
fn keeping_local_unlinks_a_symlinked_aside_without_touching_its_target() {
    // Discarding the incoming version removes the aside's own name. When that
    // name is a symlink, the file it points at is not the workspace's to delete.
    let workspace = Workspace::new("symlink-keep-local");
    let outside = OutsideFile::new(
        &format!("bowline-linked-{}.env", std::process::id()),
        "STRIPE_SECRET_KEY=real",
    );
    workspace.write("notes.md", "local");
    workspace.symlink(&aside_of("notes.md"), &outside.path);

    resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("notes.md")),
        ConflictResolution::KeepLocal,
    )
    .expect("resolve");

    assert!(list_conflicts(&workspace.root).expect("scan").is_empty());
    assert!(outside.exists(), "the symlink's target survived");
    assert_eq!(workspace.read("notes.md"), "local");
}

#[test]
fn an_aside_symlinked_outside_the_workspace_is_reported_not_read() {
    // The scan accepts an aside by NAME, so a symlink can wear one. Reading it
    // for display would print whatever the user can read — the exact secret
    // disclosure the no-follow boundary exists to deny.
    let workspace = Workspace::new("symlink-side");
    let secret = OutsideFile::new(
        &format!("bowline-secret-{}.pem", std::process::id()),
        "BEGIN OPENSSH PRIVATE KEY",
    );
    workspace.symlink(&aside_of("notes.md"), &secret.path);

    let side = read_conflict_side(
        &workspace.root,
        &WorkspacePath::new(aside_of("notes.md")),
        1_000_000,
    )
    .expect("a workspace path");

    assert_eq!(side, ConflictSide::Symlink);
}

#[test]
fn a_symlink_inside_the_workspace_is_still_not_read_as_text() {
    // pnpm-style intra-workspace links are ordinary synced entries; they are not
    // followed for display either, so the rule has no "trusted target" exception.
    let workspace = Workspace::new("symlink-inside");
    workspace.write("packages/ui/index.ts", "export {}\n");
    workspace.symlink(
        &aside_of("node_modules/ui"),
        Path::new("../packages/ui/index.ts"),
    );

    let side = read_conflict_side(
        &workspace.root,
        &WorkspacePath::new(aside_of("node_modules/ui")),
        1_000_000,
    )
    .expect("a workspace path");

    assert_eq!(side, ConflictSide::Symlink);
}

#[test]
fn a_regular_file_side_still_reads_as_text() {
    let workspace = Workspace::new("text-side");
    workspace.write(&aside_of("notes.md"), "remote\n");

    let side = read_conflict_side(
        &workspace.root,
        &WorkspacePath::new(aside_of("notes.md")),
        1_000_000,
    )
    .expect("a workspace path");

    assert_eq!(side, ConflictSide::Text("remote\n".to_string()));
}

#[test]
fn a_side_above_the_ceiling_is_named_rather_than_read() {
    let workspace = Workspace::new("large-side");
    workspace.write(&aside_of("notes.md"), "0123456789");

    let side = read_conflict_side(
        &workspace.root,
        &WorkspacePath::new(aside_of("notes.md")),
        4,
    )
    .expect("a workspace path");

    assert_eq!(side, ConflictSide::TooLarge { byte_len: 10 });
}

#[test]
fn an_aside_under_a_symlinked_parent_is_refused_and_the_target_survives() {
    // The parent chain is opened component by component with `O_NOFOLLOW`, and
    // the rename/unlink runs against THAT descriptor, so a symlinked parent is
    // refused at the open rather than resolved by the mutation.
    let workspace = Workspace::new("symlinked-parent");
    let outside_root = std::env::temp_dir().join(format!(
        "bowline-outside-dir-{}-{}",
        std::process::id(),
        "symlinked-parent"
    ));
    let _ = fs::remove_dir_all(&outside_root);
    fs::create_dir_all(&outside_root).expect("outside directory");
    fs::write(outside_root.join(aside_of("notes.md")), "secret").expect("outside aside");
    workspace.symlink("linked", &outside_root);

    for resolution in [
        ConflictResolution::KeepLocal,
        ConflictResolution::TakeRemote,
    ] {
        let error = resolve_conflict(
            &workspace.root,
            &WorkspacePath::new(format!("linked/{}", aside_of("notes.md"))),
            resolution,
        )
        .expect_err("a symlinked parent is not descended");
        assert_eq!(error.tag(), "conflict_parent_not_a_directory", "{error}");
        // A symlinked parent is the user's own tree: no rescan makes it a real
        // directory, so an agent told to retry would loop on it forever.
        assert_eq!(error.recoverability(), CommandRecoverability::UserAction);
    }

    assert!(
        outside_root.join(aside_of("notes.md")).exists(),
        "the file outside the workspace survived",
    );
    let _ = fs::remove_dir_all(&outside_root);
}

#[test]
fn a_user_file_wearing_the_marker_is_neither_listed_nor_resolvable() {
    // `notes.bowline-conflict.template` is a file a user may well have written.
    // Listing it would offer two destructive commands against it: `--keep-local`
    // deletes it, `--take-remote` renames it over `notes`.
    let workspace = Workspace::new("marker-lookalike");
    workspace.write("notes", "local");
    let lookalikes = [
        "notes.bowline-conflict.template",
        "notes.bowline-conflict.DEADBEEF",
        "notes.bowline-conflict.deadbee",
        "notes.bowline-conflict.deadbeef1",
        "notes.bowline-conflict.deadbeef.x",
        "notes.bowline-conflict.deadbeef.02",
        "notes.bowline-conflict.deadbeef.1",
    ];
    for name in lookalikes {
        workspace.write(name, "mine");
    }

    assert!(
        list_conflicts(&workspace.root).expect("scan").is_empty(),
        "a user's own files are not conflicts",
    );
    for name in lookalikes {
        for resolution in [
            ConflictResolution::KeepLocal,
            ConflictResolution::TakeRemote,
        ] {
            let error = resolve_conflict(&workspace.root, &WorkspacePath::new(name), resolution)
                .expect_err("a user file is not resolvable");
            assert_eq!(error.tag(), "not_a_conflict_aside", "{name}: {error}");
        }
        assert_eq!(workspace.read(name), "mine", "{name} survived untouched");
    }
    assert_eq!(workspace.read("notes"), "local");
}

#[test]
fn a_generated_name_and_its_collision_alternative_are_both_resolvable() {
    // The other half of the grammar: tightening it must not hide the engine's
    // own asides, including the `.2` alternative written when a name is taken.
    let workspace = Workspace::new("collision-suffix");
    workspace.write("notes.md", "local");
    workspace.write(&aside_of("notes.md"), "remote");
    workspace.write(&format!("{}.2", aside_of("notes.md")), "remote-two");

    let conflicts = list_conflicts(&workspace.root).expect("scan");
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.aside.as_str())
            .collect::<Vec<_>>(),
        vec![aside_of("notes.md"), format!("{}.2", aside_of("notes.md"))],
    );
    assert!(
        conflicts
            .iter()
            .all(|conflict| conflict.origin.as_str() == "notes.md")
    );

    resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(format!("{}.2", aside_of("notes.md"))),
        ConflictResolution::TakeRemote,
    )
    .expect("resolve");

    assert_eq!(workspace.read("notes.md"), "remote-two");
}

#[test]
fn an_already_reconciled_aside_reports_that_it_is_gone() {
    let workspace = Workspace::new("missing-aside");

    let error = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        ConflictResolution::KeepLocal,
    )
    .expect_err("nothing to resolve");

    assert_eq!(error.tag(), "no_such_conflict_aside");
}

/// A conflict named directly is not the same question as a conflict found by
/// walking: the walk prunes dependency, generated, cache and local-only
/// subtrees, stops at a descriptor depth, and gives up at an entry budget.
#[test]
fn a_conflict_the_scan_prunes_is_still_reachable_by_name() {
    let workspace = Workspace::new("pruned-subtree");
    workspace.write("node_modules/ui/index.ts", "local");
    workspace.write(&aside_of("node_modules/ui/index.ts"), "remote");
    let aside = WorkspacePath::new(aside_of("node_modules/ui/index.ts"));

    assert!(
        list_conflicts(&workspace.root).expect("scan").is_empty(),
        "the scan is expected to prune this subtree",
    );

    let located = conflict_at(&workspace.root, &aside).expect("named directly rather than scanned");
    assert_eq!(located.origin.as_str(), "node_modules/ui/index.ts");
    assert!(!located.origin_missing);

    let resolved = resolve_conflict(&workspace.root, &aside, ConflictResolution::TakeRemote)
        .expect("the mutating verb acts on exactly what the locator reached");
    assert_eq!(resolved.conflict, located);
}

#[test]
fn taking_remote_on_a_folder_is_a_user_action_rather_than_a_retry() {
    let workspace = Workspace::new("directory-aside");
    workspace.write("acme/web/src/auth.ts", "local");
    workspace.write(
        &format!("{}/inner.ts", aside_of("acme/web/src/auth.ts")),
        "remote",
    );

    let error = resolve_conflict(
        &workspace.root,
        &WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        ConflictResolution::TakeRemote,
    )
    .expect_err("a folder is not renamed over the file");

    assert_eq!(error.tag(), "conflict_aside_is_a_directory", "{error}");
    // Only the user can split a folder conflict into the files inside it, so a
    // caller told to retry would re-run this same refusal forever.
    assert_eq!(error.recoverability(), CommandRecoverability::UserAction);
}

#[test]
fn a_truncated_scan_is_never_reported_as_retryable() {
    // The entry budget is a constant, so an identical rescan of an unchanged
    // tree stops in exactly the same place.
    let error = ConflictError::ScanTruncated {
        visited: MAX_CONFLICT_SCAN_ENTRIES,
    };

    assert_eq!(error.recoverability(), CommandRecoverability::UserAction);
}

fn conflict_at_path(origin: &str) -> ConflictAside {
    ConflictAside {
        origin: WorkspacePath::new(origin.to_string()),
        aside: WorkspacePath::new(aside_of(origin)),
        origin_missing: false,
    }
}

#[test]
fn the_workspace_root_narrows_nothing_rather_than_matching_nothing() {
    // Relative to the root the prefix is empty, and no aside is under an empty
    // prefix followed by `/`: as a scope it filtered every conflict out, and the
    // surface whose whole job is listing conflicts reported none.
    assert_eq!(ProjectScope::new(""), None);
    assert_eq!(ProjectScope::new("/"), None);
    assert!(in_project_scope(
        &conflict_at_path("acme/web/src/auth.ts"),
        None
    ));
}

#[test]
fn a_scope_holds_its_own_subtree_and_no_sibling_that_shares_its_name() {
    let scope = ProjectScope::new("acme/web").expect("a narrowing below the root");

    assert_eq!(scope.as_str(), "acme/web");
    assert!(scope.contains(&conflict_at_path("acme/web/src/auth.ts")));
    assert!(!scope.contains(&conflict_at_path("acme/web-legacy/src/auth.ts")));
    assert!(!scope.contains(&conflict_at_path("other/api/src/auth.ts")));
}

#[test]
fn a_scope_that_names_the_conflicted_file_itself_holds_its_conflict() {
    // `--project` documents a "project or path", so naming the affected file is
    // the obvious way to ask about it. The directory rule alone excludes exactly
    // that aside — its remainder starts with the marker, not `/` — so the surface
    // whose job is listing that file's conflict reported none.
    let scope = ProjectScope::new("acme/web/src/auth.ts").expect("a narrowing below the root");

    assert!(scope.contains(&conflict_at_path("acme/web/src/auth.ts")));
    assert!(!scope.contains(&conflict_at_path("acme/web/src/auth.ts.bak")));
    assert!(!scope.contains(&conflict_at_path("acme/web/src/other.ts")));

    // An aside of an aside reports the aside it displaced, so the chain is walked
    // back to the source file the user actually named.
    let nested = ConflictAside {
        origin: WorkspacePath::new(aside_of("acme/web/src/auth.ts")),
        aside: WorkspacePath::new(aside_of(&aside_of("acme/web/src/auth.ts"))),
        origin_missing: false,
    };
    assert!(scope.contains(&nested));
}
