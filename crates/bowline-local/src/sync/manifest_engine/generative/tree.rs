//! Generated workspace trees and the mutations that perturb them.
//!
//! This is the input alphabet of the CanopyCheck-style convergence property: one
//! base tree is generated, then perturbed independently into a local and a
//! remote variant. Everything here is a value — generation, shrinking, and
//! replay all operate on [`TreeSpec`]/[`Mutation`] before any engine or
//! filesystem is involved, which is what makes a failing case minimizable.
//!
//! The name pools are deliberately small and disjoint: directory segments never
//! carry an extension and leaf names always do, so a generated path can never be
//! the parent directory of another generated path. That removes the file-vs-
//! directory collision class from this property on purpose — it is a separate
//! contract with its own hand-written tests, and folding it in here would make
//! every convergence failure ambiguous.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::super::manifest::{FileMode, WorkspacePath};
use super::super::push::ENGINE_STATE_DIR;
use super::rng::Rng;

const DIR_NAMES: [&str; 4] = ["alpha", "beta", "gamma", "proj"];
const LEAF_NAMES: [&str; 6] = [
    "one.txt", "two.txt", "three.rs", "four.rs", "notes.md", "data.bin",
];
/// A small body pool, so two devices independently writing the "same" content is
/// a frequent outcome. That is what exercises the adopt-without-rewrite rows of
/// the merge matrix, which a large random-bytes pool would almost never reach.
const BODIES: [&[u8]; 6] = [
    b"alpha",
    b"beta",
    b"gamma\n",
    b"",
    b"0123456789",
    b"delta delta delta",
];
const MODES: [u32; 2] = [0o644, 0o755];
/// Leaf names reserved for planted FIFOs, disjoint from [`LEAF_NAMES`] so an
/// unsyncable object can never occupy a path the tree also generates as a file.
/// Keeping the two alphabets apart is what lets the convergence property stay
/// exact: neither replica ever holds a regular file at one of these names, so
/// the fixpoint comparison is unaffected by the fault being injected.
const UNSYNCABLE_NAMES: [&str; 2] = ["queue.fifo", "socket.pipe"];

/// The content and permissions of one generated file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileBody {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mode: FileMode,
}

impl FileBody {
    fn generate(rng: &mut Rng) -> Self {
        let bytes = rng.pick(&BODIES).copied().unwrap_or(b"alpha").to_vec();
        let mode = FileMode::new(rng.pick(&MODES).copied().unwrap_or(0o644));
        Self { bytes, mode }
    }
}

/// A whole workspace as a value: every regular file, its bytes, and its mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TreeSpec {
    files: BTreeMap<WorkspacePath, FileBody>,
}

impl TreeSpec {
    pub(crate) fn generate(rng: &mut Rng, max_files: u32) -> Self {
        let count = rng.in_range(1, max_files.max(1));
        let mut files = BTreeMap::new();
        for _ in 0..count {
            if let Some(path) = generate_path(rng) {
                files.insert(path, FileBody::generate(rng));
            }
        }
        Self { files }
    }

    pub(crate) fn files(&self) -> &BTreeMap<WorkspacePath, FileBody> {
        &self.files
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn paths(&self) -> Vec<WorkspacePath> {
        self.files.keys().cloned().collect()
    }

    pub(crate) fn without(&self, path: &WorkspacePath) -> Self {
        let mut clone = self.clone();
        clone.files.remove(path);
        clone
    }

    /// Every distinct byte string the tree holds. The durability property is
    /// stated over content, not paths: a conflict-aside relocates bytes, and
    /// relocation is allowed — silent loss is not.
    pub(crate) fn bodies(&self) -> BTreeSet<Vec<u8>> {
        self.files.values().map(|body| body.bytes.clone()).collect()
    }

    pub(crate) fn materialize(&self, root: &Path) -> io::Result<()> {
        for (path, body) in &self.files {
            write_file(root, path, body)?;
        }
        Ok(())
    }

    /// Read a real workspace back into a value, skipping the engine's own
    /// private state directory (which is not workspace content and is not
    /// expected to match between devices).
    pub(crate) fn read_from_disk(root: &Path) -> io::Result<Self> {
        let mut files = BTreeMap::new();
        collect(root, "", &mut files)?;
        Ok(Self { files })
    }
}

/// One perturbation of a tree. Three of the four are the operations the merge
/// matrix distinguishes on the local side (content change, deletion, mode-only
/// change), so a generated case maps onto named matrix rows rather than arbitrary
/// filesystem noise. The fourth is the fault class this property exists to keep
/// covered: an object Bowline can never represent, appearing mid-run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mutation {
    Write {
        path: WorkspacePath,
        body: FileBody,
    },
    Remove {
        path: WorkspacePath,
    },
    Chmod {
        path: WorkspacePath,
        mode: FileMode,
    },
    /// Plant a FIFO in the workspace. A device that meets one must record it and
    /// keep syncing everything else; the whole `Fatal`-by-omission bug class
    /// shows up here as a `ViolationKind::Fault` on the very next cycle.
    PlantUnsyncable {
        path: WorkspacePath,
    },
}

impl Mutation {
    pub(crate) fn apply_to_disk(&self, root: &Path) -> io::Result<()> {
        match self {
            Self::Write { path, body } => write_file(root, path, body),
            Self::Remove { path } => match fs::remove_file(root.join(path.as_str())) {
                Ok(()) => Ok(()),
                // A generated Remove can target a path an earlier generated
                // Remove already took; that is a valid no-op, not a failure.
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
            Self::Chmod { path, mode } => {
                let target = root.join(path.as_str());
                if !target.exists() {
                    return Ok(());
                }
                fs::set_permissions(&target, fs::Permissions::from_mode(mode.get()))
            }
            Self::PlantUnsyncable { path } => {
                super::super::engine_test_support::plant_fifo(root, path.as_str())
            }
        }
    }
}

/// Generate `count` mutations against `spec`. Writes may land on a fresh path
/// (a create) or an existing one (an edit); removes and chmods only target
/// paths the tree actually holds.
pub(crate) fn generate_mutations(rng: &mut Rng, spec: &TreeSpec, count: u32) -> Vec<Mutation> {
    let existing = spec.paths();
    let mut mutations = Vec::new();
    for _ in 0..count {
        let Some(mutation) = generate_one(rng, &existing) else {
            continue;
        };
        mutations.push(mutation);
    }
    mutations
}

fn generate_one(rng: &mut Rng, existing: &[WorkspacePath]) -> Option<Mutation> {
    // Rare on purpose: this fault must be present often enough across a storm of
    // cases to catch a regression, and rare enough that most cases still explore
    // the merge matrix rather than the refusal path.
    if rng.chance(1, 8) {
        return Some(Mutation::PlantUnsyncable {
            path: generate_unsyncable_path(rng)?,
        });
    }
    let target = if existing.is_empty() || rng.chance(1, 4) {
        generate_path(rng)?
    } else {
        rng.pick(existing)?.clone()
    };
    let known = existing.contains(&target);
    if !known {
        return Some(Mutation::Write {
            path: target,
            body: FileBody::generate(rng),
        });
    }
    match rng.in_range(0, 2) {
        0 => Some(Mutation::Remove { path: target }),
        1 => Some(Mutation::Chmod {
            path: target,
            mode: FileMode::new(rng.pick(&MODES).copied().unwrap_or(0o644)),
        }),
        _ => Some(Mutation::Write {
            path: target,
            body: FileBody::generate(rng),
        }),
    }
}

fn generate_path(rng: &mut Rng) -> Option<WorkspacePath> {
    let depth = rng.in_range(0, 2);
    let mut segments = Vec::new();
    for _ in 0..depth {
        segments.push((*rng.pick(&DIR_NAMES)?).to_string());
    }
    segments.push((*rng.pick(&LEAF_NAMES)?).to_string());
    Some(WorkspacePath::new(segments.join("/")))
}

fn generate_unsyncable_path(rng: &mut Rng) -> Option<WorkspacePath> {
    let depth = rng.in_range(0, 2);
    let mut segments = Vec::new();
    for _ in 0..depth {
        segments.push((*rng.pick(&DIR_NAMES)?).to_string());
    }
    segments.push((*rng.pick(&UNSYNCABLE_NAMES)?).to_string());
    Some(WorkspacePath::new(segments.join("/")))
}

fn write_file(root: &Path, path: &WorkspacePath, body: &FileBody) -> io::Result<()> {
    let target = root.join(path.as_str());
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, &body.bytes)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(body.mode.get()))
}

fn collect(
    root: &Path,
    relative: &str,
    files: &mut BTreeMap<WorkspacePath, FileBody>,
) -> io::Result<()> {
    let dir = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if relative.is_empty() && name == ENGINE_STATE_DIR {
            continue;
        }
        let child = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        let metadata = fs::symlink_metadata(root.join(&child))?;
        if metadata.is_dir() {
            collect(root, &child, files)?;
        } else if metadata.is_file() {
            files.insert(
                WorkspacePath::new(child.clone()),
                FileBody {
                    bytes: fs::read(root.join(&child))?,
                    mode: FileMode::new(metadata.permissions().mode() & 0o777),
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::rng::{Rng, Seed};
    use super::{Mutation, TreeSpec};
    use crate::workspace::TempWorkspace;

    #[test]
    fn a_generated_path_is_never_the_parent_of_another() {
        let mut rng = Rng::from_seed(Seed::new(99));
        let spec = TreeSpec::generate(&mut rng, 24);
        let paths = spec.paths();
        for outer in &paths {
            for inner in &paths {
                assert!(
                    outer == inner || !inner.as_str().starts_with(&format!("{}/", outer.as_str())),
                    "{} is a directory prefix of {}",
                    outer.as_str(),
                    inner.as_str()
                );
            }
        }
    }

    #[test]
    fn a_materialized_tree_reads_back_identically() {
        let workspace = TempWorkspace::new("gen-tree-roundtrip").expect("temp workspace");
        let mut rng = Rng::from_seed(Seed::new(5));
        let spec = TreeSpec::generate(&mut rng, 8);
        spec.materialize(workspace.root()).expect("materialize");

        assert_eq!(
            TreeSpec::read_from_disk(workspace.root()).expect("read back"),
            spec
        );
    }

    /// An injected fault that is never generated is not coverage. This pins both
    /// halves of the contract: the storm really does plant unsyncable objects,
    /// and a planted one can never occupy a path the tree also generates as a
    /// regular file (which would make the convergence comparison ambiguous).
    #[test]
    fn a_storm_plants_unsyncable_objects_at_paths_no_file_can_occupy() {
        let mut rng = Rng::from_seed(Seed::new(2954714861));
        let mut planted = 0_u32;
        let mut file_paths = std::collections::BTreeSet::new();
        let mut planted_paths = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let spec = TreeSpec::generate(&mut rng, 8);
            file_paths.extend(spec.paths());
            for mutation in super::generate_mutations(&mut rng, &spec, 6) {
                match mutation {
                    Mutation::PlantUnsyncable { path } => {
                        planted += 1;
                        planted_paths.insert(path);
                    }
                    Mutation::Write { path, .. } => {
                        file_paths.insert(path);
                    }
                    Mutation::Remove { .. } | Mutation::Chmod { .. } => {}
                }
            }
        }
        assert!(
            planted > 0,
            "the storm must actually plant unsyncable objects"
        );
        assert!(
            planted_paths.is_disjoint(&file_paths),
            "a planted FIFO must never share a path with a generated file"
        );
    }

    #[test]
    fn removing_an_absent_path_is_a_no_op() {
        let workspace = TempWorkspace::new("gen-tree-remove-absent").expect("temp workspace");
        let mutation = Mutation::Remove {
            path: super::WorkspacePath::new("alpha/one.txt"),
        };
        mutation.apply_to_disk(workspace.root()).expect("no-op");
    }
}
