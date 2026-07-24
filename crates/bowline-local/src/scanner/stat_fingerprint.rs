//! Filesystem stat primitives for the project/Git-health scanner. Moved here
//! from the deleted old-sync `stat_cache` module: the scanner (a non-sync
//! consumer) is the surviving owner of stat fingerprints.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileTimestampNanos(i64);

impl FileTimestampNanos {
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFingerprint {
    pub size: u64,
    pub mtime_ns: FileTimestampNanos,
    pub ctime_ns: FileTimestampNanos,
    pub inode: u64,
    pub dev: u64,
    pub file_mode: u32,
}

pub fn path_is_under_any_root(path: &str, roots: &BTreeSet<String>) -> bool {
    roots.iter().any(|root| path_is_under_root(path, root))
}

fn path_is_under_root(path: &str, root: &str) -> bool {
    if root.is_empty() || path == root {
        return true;
    }
    // Equivalent to `path.starts_with(&format!("{root}/"))` without the per-call
    // allocation. '/' is ASCII, so the byte check is UTF-8 safe.
    path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}
