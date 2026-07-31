use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// A scratch tree with a workspace `root` beside a `decoy` directory that
/// stands in for anywhere else on the device.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("bowline-anchored-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root")).expect("workspace root");
        fs::create_dir_all(base.join("decoy")).expect("decoy directory");
        Self { base }
    }

    fn root(&self) -> PathBuf {
        self.base.join("root")
    }

    fn decoy(&self) -> PathBuf {
        self.base.join("decoy")
    }

    fn write(&self, path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, contents).expect("write file");
    }

    /// Replace `root/parent` with a symlink to the decoy directory, keeping the
    /// original tree at `root/moved`. Every PATH through `root/parent` now leads
    /// outside the workspace; a descriptor opened before the swap still refers to
    /// the original directory.
    fn divert_parent(&self) {
        fs::rename(self.root().join("parent"), self.root().join("moved")).expect("move aside");
        std::os::unix::fs::symlink(self.decoy(), self.root().join("parent")).expect("divert");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn open(root: &Path, path: &str) -> AnchoredDirectory {
    match open_containing_directory(root, &WorkspacePath::new(path)) {
        AnchoredOpen::Ready(directory) => directory,
        AnchoredOpen::Absent | AnchoredOpen::Blocked => {
            panic!("the containing directory of `{path}` should open")
        }
    }
}

fn leaf(path: &str) -> LeafName {
    LeafName::of(&WorkspacePath::new(path)).expect("a path with a final component")
}

#[test]
fn unlinking_follows_the_held_descriptor_and_not_the_path_it_was_opened_for() {
    let scratch = Scratch::new("unlink");
    scratch.write(&scratch.root().join("parent/aside"), "remote");
    scratch.write(&scratch.decoy().join("aside"), "victim");

    let directory = open(&scratch.root(), "parent/aside");
    scratch.divert_parent();

    // Control: the same mutation done BY PATH — what a check-then-mutate resolver
    // does — re-resolves `parent` and destroys the file outside the workspace.
    fs::remove_file(scratch.root().join("parent/aside")).expect("path-based removal");
    assert!(
        !scratch.decoy().join("aside").exists(),
        "the swap must actually redirect a path-based mutation, or this test proves nothing",
    );
    scratch.write(&scratch.decoy().join("aside"), "victim");

    directory.unlink(&leaf("aside")).expect("anchored unlink");

    assert_eq!(
        fs::read_to_string(scratch.decoy().join("aside")).expect("decoy file"),
        "victim",
        "the anchored unlink must not reach outside the workspace",
    );
    assert!(
        !scratch.root().join("moved/aside").exists(),
        "the anchored unlink removed the entry in the directory it holds",
    );
}

#[test]
fn renaming_lands_in_the_directory_that_was_opened_not_the_one_swapped_in() {
    let scratch = Scratch::new("rename");
    scratch.write(&scratch.root().join("parent/notes.md"), "local");
    scratch.write(&scratch.root().join("parent/notes.md.aside"), "remote");
    scratch.write(&scratch.decoy().join("notes.md"), "victim");
    scratch.write(&scratch.decoy().join("notes.md.aside"), "decoy-remote");

    let directory = open(&scratch.root(), "parent/notes.md.aside");
    scratch.divert_parent();

    // Control: renaming by path overwrites the victim outside the workspace.
    fs::rename(
        scratch.root().join("parent/notes.md.aside"),
        scratch.root().join("parent/notes.md"),
    )
    .expect("path-based rename");
    assert_eq!(
        fs::read_to_string(scratch.decoy().join("notes.md")).expect("decoy file"),
        "decoy-remote",
        "the swap must actually redirect a path-based rename, or this test proves nothing",
    );
    scratch.write(&scratch.decoy().join("notes.md"), "victim");
    scratch.write(&scratch.decoy().join("notes.md.aside"), "decoy-remote");

    directory
        .rename(&leaf("notes.md.aside"), &leaf("notes.md"))
        .expect("anchored rename");

    assert_eq!(
        fs::read_to_string(scratch.decoy().join("notes.md")).expect("decoy file"),
        "victim",
        "the anchored rename must not overwrite anything outside the workspace",
    );
    assert_eq!(
        fs::read_to_string(scratch.root().join("moved/notes.md")).expect("moved file"),
        "remote",
        "the anchored rename landed in the directory the descriptor holds",
    );
}

#[test]
fn a_tree_removal_stays_under_the_held_descriptor_and_unlinks_symlinks() {
    let scratch = Scratch::new("tree");
    scratch.write(&scratch.root().join("parent/tree/a.txt"), "a");
    scratch.write(&scratch.root().join("parent/tree/nested/deeper/b.txt"), "b");
    scratch.write(&scratch.decoy().join("tree/keep.txt"), "keep");
    std::os::unix::fs::symlink(
        scratch.decoy().join("tree"),
        scratch.root().join("parent/tree/escape"),
    )
    .expect("symlink into the decoy");

    let directory = open(&scratch.root(), "parent/tree");
    assert_eq!(
        directory.classify(&leaf("tree")).expect("classify"),
        AnchoredLeafKind::Directory,
    );
    scratch.divert_parent();

    directory.remove_tree(&leaf("tree")).expect("remove tree");

    assert!(!scratch.root().join("moved/tree").exists());
    assert_eq!(
        fs::read_to_string(scratch.decoy().join("tree/keep.txt")).expect("decoy file"),
        "keep",
        "neither the diverted parent nor the symlinked child led the removal outside",
    );
}

/// Listing is the read-side twin of the mutations above: a walk that rebuilds
/// `root.join(relative)` for each level hands `read_dir` a path whose components
/// the kernel resolves again, so a directory already classified and then swapped
/// for a symlink leads the walk outside the workspace — and the names it finds
/// out there are reported to whoever asked.
#[test]
fn a_listing_reads_the_held_descriptor_and_not_the_path_it_was_opened_for() {
    let scratch = Scratch::new("entries");
    scratch.write(&scratch.root().join("parent/inside/kept.txt"), "kept");
    scratch.write(&scratch.decoy().join("outside.txt"), "victim");

    let directory = open(&scratch.root(), "parent/inside");
    scratch.divert_parent();

    // Control: the same listing done BY PATH — what a recursive `read_dir` walk
    // does — re-resolves `parent` and reads the directory outside the workspace.
    let by_path: Vec<String> = fs::read_dir(scratch.root().join("parent"))
        .expect("path-based listing")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        by_path.contains(&"outside.txt".to_string()),
        "the swap must actually redirect a path-based listing, or this test proves nothing",
    );

    let listed: Vec<AnchoredEntry> = directory.entries().expect("anchored listing");

    assert_eq!(
        listed
            .iter()
            .filter_map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["inside"],
        "the anchored listing must not reach outside the workspace",
    );
    assert_eq!(listed[0].kind, AnchoredLeafKind::Directory);
    assert_eq!(
        listed[0].byte_len, None,
        "a directory's length is bookkeeping, not content",
    );
}

#[test]
fn a_child_swapped_for_a_symlink_after_the_listing_is_never_descended() {
    let scratch = Scratch::new("descend");
    scratch.write(&scratch.root().join("parent/inside/kept.txt"), "kept");
    scratch.write(&scratch.decoy().join("outside.txt"), "victim");

    let directory = open(&scratch.root(), "parent/inside");
    let listed = directory.entries().expect("anchored listing");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].kind, AnchoredLeafKind::Directory);

    // The listing classified `inside` as a real directory; a concurrent process
    // replaces it with a symlink out of the workspace before it is descended.
    fs::remove_dir_all(scratch.root().join("parent/inside")).expect("remove the real directory");
    std::os::unix::fs::symlink(scratch.decoy(), scratch.root().join("parent/inside"))
        .expect("divert the child");

    // Control: re-deriving the child's path and listing it reads the decoy.
    assert!(
        fs::read_dir(scratch.root().join("parent/inside"))
            .expect("path-based descent")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == "outside.txt"),
        "the swap must actually redirect a path-based descent, or this test proves nothing",
    );

    assert!(matches!(
        directory.open_directory(&leaf("inside")),
        AnchoredOpen::Blocked,
    ));
}

#[test]
fn a_listed_file_carries_the_length_its_own_stat_reported() {
    let scratch = Scratch::new("sizes");
    scratch.write(&scratch.root().join("parent/notes.md"), "0123456789");

    let listed = open(&scratch.root(), "parent/notes.md")
        .entries()
        .expect("anchored listing");

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].kind, AnchoredLeafKind::NonDirectory);
    assert_eq!(listed[0].byte_len, Some(10));
}

/// A workspace on an external volume is commonly reached through a symlinked
/// `~/Code`. The root is the trust anchor the caller named, so it is followed;
/// everything below it is not.
#[test]
fn a_symlinked_workspace_root_still_opens() {
    let scratch = Scratch::new("symlinked-root");
    scratch.write(&scratch.root().join("parent/notes.md"), "local");
    let link = scratch.base.join("link-to-root");
    std::os::unix::fs::symlink(scratch.root(), &link).expect("symlink the root");

    assert!(matches!(
        open_containing_directory(&link, &WorkspacePath::new("parent/notes.md")),
        AnchoredOpen::Ready(_),
    ));
}

#[test]
fn a_symlinked_intermediate_component_blocks_the_open() {
    let scratch = Scratch::new("blocked");
    scratch.write(&scratch.decoy().join("aside"), "victim");
    std::os::unix::fs::symlink(scratch.decoy(), scratch.root().join("parent")).expect("symlink");

    assert!(matches!(
        open_containing_directory(&scratch.root(), &WorkspacePath::new("parent/aside")),
        AnchoredOpen::Blocked,
    ));
}

#[test]
fn a_missing_intermediate_component_reports_the_chain_absent() {
    // Anchored mutations act on entries that already exist; nothing is created
    // on the way down. A missing chain means the leaf cannot exist either, which
    // is an ordinary answer rather than the refusal a symlinked chain earns.
    let scratch = Scratch::new("missing");

    assert!(matches!(
        open_containing_directory(&scratch.root(), &WorkspacePath::new("absent/aside")),
        AnchoredOpen::Absent,
    ));
}

#[test]
fn an_entry_at_the_workspace_root_anchors_on_the_root_itself() {
    let scratch = Scratch::new("root-leaf");
    scratch.write(&scratch.root().join("notes.md"), "local");

    let directory = open(&scratch.root(), "notes.md");

    assert_eq!(
        directory.classify(&leaf("notes.md")).expect("classify"),
        AnchoredLeafKind::NonDirectory,
    );
    assert_eq!(
        directory.classify(&leaf("absent.md")).expect("classify"),
        AnchoredLeafKind::Absent,
    );
}

#[test]
fn a_leaf_name_is_the_final_component_and_never_a_path() {
    assert_eq!(
        LeafName::of(&WorkspacePath::new("a/b/c.txt")),
        LeafName::of(&WorkspacePath::new("c.txt")),
    );
    // A name that would make an anchored operation resolve a component again has
    // no leaf at all, so it can never be handed to one.
    assert_eq!(LeafName::of(&WorkspacePath::new("a/b/")), None);
    assert_eq!(LeafName::of(&WorkspacePath::new("")), None);
    assert!(is_recovery_owner_record_name(
        ".bowline-recovery-owner-0123456789abcdef0123456789abcdef.record"
    ));
    assert!(is_recovery_owner_record_name(
        ".bowline-recovery-owner-0123456789abcdef0123456789abcdef.record.complete"
    ));
    assert!(!is_recovery_owner_record_name(
        ".bowline-recovery-owner-notes.record"
    ));
}

#[test]
fn atomic_write_never_reuses_a_predictable_hard_link() {
    let scratch = Scratch::new("atomic-write-hard-link");
    scratch.write(&scratch.root().join("victim"), "preserve me");
    fs::hard_link(
        scratch.root().join("victim"),
        scratch.root().join(".state.tmp"),
    )
    .expect("seed predictable hard link");
    let directory = open_workspace_root(&scratch.root()).expect("destination");

    let outcome = directory
        .write_private_file_atomic(&leaf("state"), b"new state")
        .expect("atomic write");

    assert!(matches!(outcome, AtomicWrite::Written));
    assert_eq!(
        fs::read(scratch.root().join("victim")).expect("victim"),
        b"preserve me"
    );
    assert_eq!(
        fs::read(scratch.root().join(".state.tmp")).expect("hard link"),
        b"preserve me"
    );
    assert_eq!(
        fs::read(scratch.root().join("state")).expect("written state"),
        b"new state"
    );
}

#[test]
fn atomic_write_cleanup_reclaims_only_generated_stale_temps() {
    let scratch = Scratch::new("atomic-write-cleanup");
    let directory = open_workspace_root(&scratch.root()).expect("destination");
    let generated = leaf(".bowline-materialize-atomic-00000000000000000000000000000000.tmp");
    let lookalike = leaf(".bowline-materialize-atomic-not-user-state.tmp");
    scratch.write(
        &scratch.root().join(generated.as_str().expect("generated")),
        "stale",
    );
    scratch.write(
        &scratch.root().join(lookalike.as_str().expect("lookalike")),
        "user",
    );

    directory
        .clean_atomic_write_temps_before(u64::MAX)
        .expect("clean stale atomic temp");

    assert!(
        !scratch
            .root()
            .join(generated.as_str().expect("generated"))
            .exists()
    );
    assert_eq!(
        fs::read(scratch.root().join(lookalike.as_str().expect("lookalike")))
            .expect("preserved lookalike"),
        b"user"
    );
}

#[test]
fn cross_device_fallback_stages_then_atomically_renames() {
    let scratch = Scratch::new("cross-device-atomic");
    let source_path = scratch.base.join("staged");
    scratch.write(&source_path, "recovered bytes");
    scratch.write(
        &scratch.root().join(".bowline-recovery-user-data.tmp"),
        "user bytes",
    );
    scratch.write(
        &scratch
            .root()
            .join(".bowline-recovery-00000000000000000000000000000000.tmp"),
        "abandoned recovery bytes",
    );
    let mut source = fs::File::open(&source_path).expect("staged source");
    let destination = open_workspace_root(&scratch.root()).expect("destination");
    let abandoned = leaf(".bowline-recovery-00000000000000000000000000000000.tmp");
    let owner = abandoned.recovery_owner_sibling().expect("recovery owner");
    let mut owner_file = destination
        .create_private_file(&owner)
        .expect("owner record");
    write_recovery_owner(
        &mut owner_file,
        0,
        directory_identity(&destination.directory).expect("directory identity"),
        Some(file_identity(
            &fs::metadata(
                scratch
                    .root()
                    .join(".bowline-recovery-00000000000000000000000000000000.tmp"),
            )
            .expect("abandoned metadata"),
        )),
        &leaf("restored.txt"),
    )
    .expect("write owner");

    let outcome = destination
        .copy_staged_file_atomic(
            &destination,
            &mut source,
            &leaf("restored.txt"),
            FileMode::new(0o640),
        )
        .expect("atomic fallback");

    assert!(matches!(outcome, GuardedWrite::Written(_)));
    assert_eq!(
        fs::read(scratch.root().join("restored.txt")).expect("installed"),
        b"recovered bytes"
    );
    assert_eq!(
        fs::read(scratch.root().join(".bowline-recovery-user-data.tmp"))
            .expect("preserved matching user file"),
        b"user bytes"
    );
    assert!(
        !scratch
            .root()
            .join(".bowline-recovery-00000000000000000000000000000000.tmp")
            .exists()
    );
}

#[test]
fn recovery_cleanup_finishes_an_install_interrupted_after_exchange() {
    let scratch = Scratch::new("cross-device-interrupted-exchange");
    scratch.write(&scratch.root().join("restored.txt"), "old bytes");
    scratch.write(
        &scratch
            .root()
            .join(".bowline-recovery-00000000000000000000000000000000.tmp"),
        "recovered bytes",
    );
    let destination = open_workspace_root(&scratch.root()).expect("destination");
    let temp = leaf(".bowline-recovery-00000000000000000000000000000000.tmp");
    let destination_leaf = leaf("restored.txt");
    let temp_identity = file_identity(
        &fs::metadata(
            scratch
                .root()
                .join(".bowline-recovery-00000000000000000000000000000000.tmp"),
        )
        .expect("temp metadata"),
    );
    let owner = temp.recovery_owner_sibling().expect("recovery owner");
    let mut owner_file = destination
        .create_private_file(&owner)
        .expect("owner record");
    write_recovery_owner(
        &mut owner_file,
        0,
        directory_identity(&destination.directory).expect("directory identity"),
        Some(temp_identity),
        &destination_leaf,
    )
    .expect("write owner");
    rustix::fs::renameat_with(
        &destination.directory,
        temp.as_c_str(),
        &destination.directory,
        destination_leaf.as_c_str(),
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .expect("simulate interrupted exchange");

    destination
        .clean_owned_recovery_temps(&destination)
        .expect("finish interrupted cleanup");

    assert_eq!(
        fs::read(scratch.root().join("restored.txt")).expect("installed destination"),
        b"recovered bytes"
    );
    assert!(
        !scratch
            .root()
            .join(temp.as_str().expect("utf-8 temp"))
            .exists()
    );
    assert!(
        !scratch
            .root()
            .join(owner.as_str().expect("utf-8 owner"))
            .exists()
    );
}

#[test]
fn recovery_cleanup_finishes_stale_pending_records() {
    let scratch = Scratch::new("cross-device-stale-pending");
    let destination = open_workspace_root(&scratch.root()).expect("destination");
    let directory_identity =
        directory_identity(&destination.directory).expect("directory identity");
    let destination_leaf = leaf("restored.txt");
    let abandoned_temp = leaf(".bowline-recovery-00000000000000000000000000000000.tmp");
    let abandoned_owner = abandoned_temp
        .recovery_owner_sibling()
        .expect("abandoned owner");
    destination
        .create_private_file(&abandoned_temp)
        .expect("empty pending temp");
    let mut abandoned_owner_file = destination
        .create_private_file(&abandoned_owner)
        .expect("abandoned owner record");
    write_recovery_owner(
        &mut abandoned_owner_file,
        0,
        directory_identity,
        None,
        &destination_leaf,
    )
    .expect("write abandoned owner");
    let absent_temp = leaf(".bowline-recovery-11111111111111111111111111111111.tmp");
    let absent_owner = absent_temp.recovery_owner_sibling().expect("absent owner");
    let mut absent_owner_file = destination
        .create_private_file(&absent_owner)
        .expect("absent owner record");
    write_recovery_owner(
        &mut absent_owner_file,
        0,
        directory_identity,
        None,
        &destination_leaf,
    )
    .expect("write absent owner");

    destination
        .clean_owned_recovery_temps(&destination)
        .expect("finish stale pending cleanup");

    for stale_leaf in [&abandoned_temp, &abandoned_owner, &absent_owner] {
        assert!(
            !scratch
                .root()
                .join(stale_leaf.as_str().expect("utf-8 leaf"))
                .exists(),
            "{} should be removed",
            stale_leaf.as_str().expect("utf-8 leaf")
        );
    }
}

#[test]
fn cross_device_fallback_rejects_a_substituted_temp_inode() {
    let scratch = Scratch::new("cross-device-substitution");
    scratch.write(&scratch.root().join("restored.txt"), "local bytes");
    let destination = open_workspace_root(&scratch.root()).expect("destination");
    let temp = leaf(".bowline-recovery-race.tmp");
    let mut copied = destination.create_private_file(&temp).expect("copied temp");
    copied.write_all(b"recovered bytes").expect("copy bytes");
    copied.sync_all().expect("sync copied temp");

    fs::rename(
        scratch.root().join(".bowline-recovery-race.tmp"),
        scratch.root().join("detached-trusted-copy"),
    )
    .expect("detach copied inode");
    scratch.write(
        &scratch.root().join(".bowline-recovery-race.tmp"),
        "substituted bytes",
    );

    let outcome = destination
        .install_copied_temp(&copied, &temp, &leaf("restored.txt"))
        .expect("guarded install");

    assert!(matches!(outcome, GuardedWrite::Blocked));
    assert_eq!(
        fs::read(scratch.root().join("restored.txt")).expect("preserved destination"),
        b"local bytes"
    );
    assert!(!scratch.root().join(".bowline-recovery-race.tmp").exists());
}
