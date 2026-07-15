//! Filesystem access shim shared by the scanner and policy loader.
//!
//! In test builds it records directory-read and metadata calls so
//! traversal-cost invariants can be asserted — most importantly that a
//! root-shallow tick performs a single `read_dir` of the workspace root and
//! never descends into subdirectories for scanning, policy discovery, or Git
//! health collection. In release builds it is a thin passthrough with no
//! bookkeeping. This is not a product surface; it exists only to prove the
//! traversal claim.

use std::{fs, io, path::Path};

pub(crate) fn read_dir(path: &Path) -> io::Result<fs::ReadDir> {
    #[cfg(test)]
    probe::note_read_dir(path);
    fs::read_dir(path)
}

pub(crate) fn symlink_metadata(path: &Path) -> io::Result<fs::Metadata> {
    #[cfg(test)]
    probe::note_metadata();
    fs::symlink_metadata(path)
}

#[cfg(test)]
pub(crate) use probe::{install, take};

#[cfg(test)]
mod probe {
    use std::{
        cell::RefCell,
        path::{Path, PathBuf},
    };

    /// Directory-read and metadata call counts observed since [`install`].
    ///
    /// `read_dir` of the installed root is counted separately from any deeper
    /// directory read so a root-shallow tick can assert `subdir_read_dir_count
    /// == 0`.
    #[derive(Debug, Default, Clone, Copy)]
    pub(crate) struct TraversalCounts {
        pub root_read_dir_count: u64,
        pub subdir_read_dir_count: u64,
        pub metadata_count: u64,
    }

    struct ProbeState {
        root: PathBuf,
        counts: TraversalCounts,
    }

    thread_local! {
        static PROBE: RefCell<Option<ProbeState>> = const { RefCell::new(None) };
    }

    /// Arm the probe for the current test thread with the workspace root whose
    /// `read_dir` calls count as root-level rather than subdirectory reads.
    pub(crate) fn install(root: &Path) {
        PROBE.with(|probe| {
            *probe.borrow_mut() = Some(ProbeState {
                root: root.to_path_buf(),
                counts: TraversalCounts::default(),
            });
        });
    }

    /// Disarm the probe and return the counts accumulated since [`install`].
    pub(crate) fn take() -> TraversalCounts {
        PROBE.with(|probe| {
            probe
                .borrow_mut()
                .take()
                .map(|state| state.counts)
                .unwrap_or_default()
        })
    }

    pub(crate) fn note_read_dir(path: &Path) {
        PROBE.with(|probe| {
            if let Some(state) = probe.borrow_mut().as_mut() {
                if path == state.root {
                    state.counts.root_read_dir_count += 1;
                } else {
                    state.counts.subdir_read_dir_count += 1;
                }
            }
        });
    }

    pub(crate) fn note_metadata() {
        PROBE.with(|probe| {
            if let Some(state) = probe.borrow_mut().as_mut() {
                state.counts.metadata_count += 1;
            }
        });
    }
}
