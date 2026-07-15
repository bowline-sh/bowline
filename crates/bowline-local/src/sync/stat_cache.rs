use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::{ContentId, WorkspaceId};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::ScanScope;
use super::change_index::LocalChangeIndex;
use crate::metadata::{MetadataError, MetadataStore};

/// Bump to wholesale-invalidate every cached row after any change to what a
/// row means. Mismatched rows are misses and overwritten, never migrated.
pub const STAT_CACHE_FORMAT_VERSION: u32 = 1;
pub const VERIFY_SHARD_COUNT: u64 = 24;
const VERIFY_SHARD_INTERVAL_SECONDS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileTimestampNanos(i64);

impl FileTimestampNanos {
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    pub fn now() -> Self {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let nanos = duration
            .as_secs()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::from(duration.subsec_nanos()));
        Self(i64::try_from(nanos).unwrap_or(i64::MAX))
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentKeyFingerprint(String);

impl ContentKeyFingerprint {
    pub fn from_content_key(content_key: &[u8; 32]) -> Self {
        let hash = blake3::hash(content_key);
        let hex = hash.to_hex();
        Self(hex[..16].to_string())
    }

    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatCacheRow {
    pub path: String,
    // Policy is intentionally absent: classification, mode, and access come
    // from the live scan every tick, so policy edits do not need cache invalidation.
    pub fingerprint: StatFingerprint,
    pub key_epoch: u32,
    pub content_key_fingerprint: ContentKeyFingerprint,
    pub content_id: ContentId,
    pub byte_len: u64,
    pub format_version: u32,
    pub hashed_at_ns: FileTimestampNanos,
    pub last_verified_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RehashReason {
    NoRow,
    FingerprintChanged,
    RacilyClean,
    ContentKeyChanged,
    FormatVersionChanged,
    ConflictOverride,
    VerifyShard,
}

impl RehashReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRow => "no-row",
            Self::FingerprintChanged => "fingerprint-changed",
            Self::RacilyClean => "racily-clean",
            Self::ContentKeyChanged => "content-key-changed",
            Self::FormatVersionChanged => "format-version-changed",
            Self::ConflictOverride => "conflict-override",
            Self::VerifyShard => "verify-shard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDecision {
    Hit {
        content_id: ContentId,
        byte_len: u64,
    },
    Rehash(RehashReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatCacheDivergence {
    pub path: String,
    pub cached_content_id: ContentId,
    pub observed_content_id: ContentId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyDecision {
    Compare { cached_content_id: ContentId },
    Rehash(RehashReason),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files_hashed: u64,
    pub stat_hits: u64,
    pub rehash_reasons: BTreeMap<RehashReason, u64>,
    pub future_mtime_paths: u64,
    pub divergence_count: u64,
}

impl ScanStats {
    fn record_rehash(&mut self, reason: RehashReason) {
        *self.rehash_reasons.entry(reason).or_insert(0) += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatCacheWriteBack {
    pub upserts: Vec<StatCacheRow>,
    pub deletes: BTreeSet<String>,
}

pub struct StatCacheSession {
    rows: BTreeMap<String, StatCacheRow>,
    scan_started_ns: FileTimestampNanos,
    key_epoch: u32,
    content_key_fingerprint: ContentKeyFingerprint,
    upserts: Vec<StatCacheRow>,
    stats: ScanStats,
}

impl StatCacheSession {
    pub fn load(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> Result<Self, MetadataError> {
        Ok(Self::from_rows(
            store.stat_cache_rows(workspace_id)?,
            key_epoch,
            content_key,
        ))
    }

    pub fn load_scoped(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        dirty_roots: &BTreeSet<String>,
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> Result<Self, MetadataError> {
        Ok(Self::from_rows(
            store.stat_cache_rows_for_roots(workspace_id, dirty_roots)?,
            key_epoch,
            content_key,
        ))
    }

    /// Plan 06 U7d — root-shallow loader. Loads only root-level cache rows through
    /// the U7b indexed root-level projection, so a workspace with a huge deep
    /// index is never scanned. Deep entries are preserved from the head manifest
    /// at the call site, not by loading their cache rows.
    pub fn load_root_level(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> Result<Self, MetadataError> {
        let mut index = LocalChangeIndex::new(store, workspace_id.clone());
        let root_paths = index.root_level_paths()?;
        Ok(Self::from_rows(
            store.stat_cache_rows_for_paths(workspace_id, &root_paths)?,
            key_epoch,
            content_key,
        ))
    }

    /// Plan 06 U7d — combined loader for `DirtySubtrees { root_shallow: true }`.
    /// Loads root-level rows plus rows under `roots` through the U7b indexed
    /// projections; unrelated deep rows are never loaded.
    pub fn load_roots_and_root_level(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        roots: &BTreeSet<String>,
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> Result<Self, MetadataError> {
        let mut index = LocalChangeIndex::new(store, workspace_id.clone());
        let mut paths = index.root_level_paths()?;
        paths.extend(index.paths_under_roots(roots)?);
        Ok(Self::from_rows(
            store.stat_cache_rows_for_paths(workspace_id, &paths)?,
            key_epoch,
            content_key,
        ))
    }

    pub fn empty_for_scan(key_epoch: u32, content_key: &[u8; 32]) -> Self {
        Self::from_rows(BTreeMap::new(), key_epoch, content_key)
    }

    fn from_rows(
        rows: BTreeMap<String, StatCacheRow>,
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> Self {
        Self {
            rows,
            scan_started_ns: FileTimestampNanos::now(),
            key_epoch,
            content_key_fingerprint: ContentKeyFingerprint::from_content_key(content_key),
            upserts: Vec::new(),
            stats: ScanStats::default(),
        }
    }

    /// Number of cache rows this session loaded. Used by budget tests to prove a
    /// root-shallow session consults O(root entries), never the deep index.
    #[cfg(test)]
    pub(crate) fn loaded_row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn decide(&mut self, path: &str, observed: &StatFingerprint) -> CacheDecision {
        let (content_id, byte_len) = match self.valid_row(path, observed) {
            Ok(row) => (row.content_id.clone(), row.byte_len),
            Err(reason) => {
                self.stats.record_rehash(reason);
                return CacheDecision::Rehash(reason);
            }
        };
        if observed.mtime_ns > self.scan_started_ns {
            self.stats.future_mtime_paths = self.stats.future_mtime_paths.saturating_add(1);
        }
        self.stats.stat_hits = self.stats.stat_hits.saturating_add(1);
        CacheDecision::Hit {
            content_id,
            byte_len,
        }
    }

    pub fn decide_verify(&mut self, path: &str, observed: &StatFingerprint) -> VerifyDecision {
        let cached_content_id = match self.valid_row(path, observed) {
            Ok(row) => row.content_id.clone(),
            Err(reason) => {
                self.stats.record_rehash(reason);
                return VerifyDecision::Rehash(reason);
            }
        };
        self.stats.record_rehash(RehashReason::VerifyShard);
        VerifyDecision::Compare { cached_content_id }
    }

    pub fn record_conflict_override(&mut self) {
        self.stats.record_rehash(RehashReason::ConflictOverride);
    }

    pub fn record_divergence(&mut self) {
        self.stats.divergence_count = self.stats.divergence_count.saturating_add(1);
    }

    pub fn record_hashed(
        &mut self,
        path: &str,
        observed: StatFingerprint,
        content_id: ContentId,
        byte_len: u64,
        last_verified_at: String,
    ) {
        self.stats.files_hashed = self.stats.files_hashed.saturating_add(1);
        self.upserts.push(StatCacheRow {
            path: path.to_string(),
            fingerprint: observed,
            key_epoch: self.key_epoch,
            content_key_fingerprint: self.content_key_fingerprint.clone(),
            content_id,
            byte_len,
            format_version: STAT_CACHE_FORMAT_VERSION,
            hashed_at_ns: self.scan_started_ns,
            last_verified_at,
        });
    }

    pub fn finish(&mut self, observed_paths: &BTreeSet<String>) -> StatCacheWriteBack {
        self.finish_with_delete_scope(observed_paths, StatCacheDeleteScope::All)
    }

    pub fn finish_scoped(
        &mut self,
        observed_paths: &BTreeSet<String>,
        dirty_roots: &BTreeSet<String>,
    ) -> StatCacheWriteBack {
        self.finish_with_delete_scope(
            observed_paths,
            StatCacheDeleteScope::UnderRoots(dirty_roots),
        )
    }

    /// Prune unobserved cached rows within `scope` only. The active scan observed
    /// every path it owns, so an unobserved owned row means the file is gone and
    /// its cache row must be deleted; rows outside `scope` were never observed
    /// this tick and must be preserved (KTD-4).
    pub fn finish_with_delete_scope(
        &mut self,
        observed_paths: &BTreeSet<String>,
        scope: StatCacheDeleteScope<'_>,
    ) -> StatCacheWriteBack {
        self.finish_with_delete_scope_matching(scope, |path| observed_paths.contains(path))
    }

    pub fn finish_with_delete_scope_matching(
        &mut self,
        scope: StatCacheDeleteScope<'_>,
        mut is_observed: impl FnMut(&str) -> bool,
    ) -> StatCacheWriteBack {
        let deletes = self
            .rows
            .keys()
            .filter(|path| scope.owns_path(path))
            .filter(|path| !is_observed(path))
            .cloned()
            .collect();
        StatCacheWriteBack {
            upserts: std::mem::take(&mut self.upserts),
            deletes,
        }
    }

    pub fn stats(&self) -> &ScanStats {
        &self.stats
    }

    fn valid_row(
        &self,
        path: &str,
        observed: &StatFingerprint,
    ) -> Result<&StatCacheRow, RehashReason> {
        let Some(row) = self.rows.get(path) else {
            return Err(RehashReason::NoRow);
        };
        if row.format_version != STAT_CACHE_FORMAT_VERSION {
            return Err(RehashReason::FormatVersionChanged);
        }
        if row.key_epoch != self.key_epoch
            || row.content_key_fingerprint != self.content_key_fingerprint
        {
            return Err(RehashReason::ContentKeyChanged);
        }
        if row.fingerprint != *observed {
            return Err(RehashReason::FingerprintChanged);
        }
        if row.fingerprint.mtime_ns >= row.hashed_at_ns {
            return Err(RehashReason::RacilyClean);
        }
        Ok(row)
    }
}

/// Which unobserved cached rows a write-back pass is allowed to prune (KTD-4).
/// Replaces the earlier `Option<&BTreeSet<String>>` sentinel — a partial scan
/// owns a specific slice of the index, and `None` (prune-all) vs `Some(roots)`
/// could not express the root-level-only and combined passes without either
/// pruning the deep index or missing deleted root-level files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatCacheDeleteScope<'a> {
    /// Full scan: every unobserved row is a deletion.
    All,
    /// Scoped subtree scan: only unobserved rows under a dirty root.
    UnderRoots(&'a BTreeSet<String>),
    /// Root-shallow scan: only unobserved root-level (`!path.contains('/')`) rows.
    RootLevelOnly,
    /// Combined tick: unobserved rows that are root-level OR under a dirty root.
    UnderRootsAndRootLevel(&'a BTreeSet<String>),
}

impl StatCacheDeleteScope<'_> {
    /// True when `path` belongs to this scope, i.e. the active scan observed the
    /// directory that would contain it, so its absence is an authoritative delete.
    pub(crate) fn owns_path(&self, path: &str) -> bool {
        match self {
            Self::All => true,
            Self::UnderRoots(roots) => path_is_under_any_root(path, roots),
            Self::RootLevelOnly => is_root_level(path),
            Self::UnderRootsAndRootLevel(roots) => {
                is_root_level(path) || path_is_under_any_root(path, roots)
            }
        }
    }
}

fn is_root_level(path: &str) -> bool {
    !path.contains('/')
}

/// True when a live scan pass for `scan_scope` re-observes `path` this tick, so
/// the path is NOT preserved from the head manifest (and a File entry there does
/// not need a packed locator to survive a partial pass). The inverse selects the
/// deep head entries a partial tick must preserve and the entries whose absence
/// is authoritative (KTD-15). A full scan observes everything; a root-shallow
/// pass observes root-level entries; a combined tick observes root-level entries
/// plus everything under the dirty roots.
pub fn path_is_live_observed(path: &str, scan_scope: &ScanScope) -> bool {
    match scan_scope {
        ScanScope::Full(_) => true,
        ScanScope::RootShallow => is_root_level(path),
        ScanScope::DirtySubtrees {
            roots,
            root_shallow,
        } => path_is_under_any_root(path, roots) || (*root_shallow && is_root_level(path)),
    }
}

pub fn path_is_under_any_root(path: &str, roots: &BTreeSet<String>) -> bool {
    roots.iter().any(|root| path_is_under_root(path, root))
}

fn path_is_under_root(path: &str, root: &str) -> bool {
    if root.is_empty() || path == root {
        return true;
    }
    // Equivalent to `path.starts_with(&format!("{root}/"))` without the per-call
    // allocation: `finish_with_delete_scope` calls this for every cached row.
    // '/' is ASCII, so the byte check is UTF-8 safe.
    path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}

pub fn verify_shard_for_timestamp(value: &str) -> u64 {
    let Ok(timestamp) = OffsetDateTime::parse(value, &Rfc3339) else {
        return 0;
    };
    timestamp
        .unix_timestamp()
        .div_euclid(VERIFY_SHARD_INTERVAL_SECONDS)
        .rem_euclid(i64::try_from(VERIFY_SHARD_COUNT).unwrap_or(1)) as u64
}

pub fn verify_shard_for_path(path: &str) -> u64 {
    let hash = blake3::hash(path.as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(bytes) % VERIFY_SHARD_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn content_key_fingerprint_mismatch_is_miss() {
        let row = row("app/main.rs", fingerprint());
        let mut session = session_with_rows([row], 1, &[8; 32]);

        assert_eq!(
            session.decide("app/main.rs", &fingerprint()),
            CacheDecision::Rehash(RehashReason::ContentKeyChanged)
        );
    }

    #[test]
    fn format_version_mismatch_is_miss() {
        let mut row = row("app/main.rs", fingerprint());
        row.format_version = STAT_CACHE_FORMAT_VERSION + 1;
        let mut session = session_with_rows([row], 1, &KEY);

        assert_eq!(
            session.decide("app/main.rs", &fingerprint()),
            CacheDecision::Rehash(RehashReason::FormatVersionChanged)
        );
    }

    #[test]
    fn racily_clean_row_mtime_at_or_after_hashed_at_forces_rehash() {
        let observed = fingerprint();
        let mut row = row("app/main.rs", observed);
        row.hashed_at_ns = observed.mtime_ns;
        let mut session = session_with_rows([row], 1, &KEY);

        assert_eq!(
            session.decide("app/main.rs", &observed),
            CacheDecision::Rehash(RehashReason::RacilyClean)
        );
    }

    #[test]
    fn rehash_when_any_fingerprint_field_changes() {
        let base = fingerprint();
        let cases = [
            StatFingerprint { size: 2, ..base },
            StatFingerprint {
                mtime_ns: FileTimestampNanos::new(2),
                ..base
            },
            StatFingerprint {
                ctime_ns: FileTimestampNanos::new(3),
                ..base
            },
            StatFingerprint { inode: 4, ..base },
            StatFingerprint { dev: 5, ..base },
            StatFingerprint {
                file_mode: 0o100755,
                ..base
            },
        ];

        for observed in cases {
            let mut session = session_with_rows([row("app/main.rs", base)], 1, &KEY);
            assert_eq!(
                session.decide("app/main.rs", &observed),
                CacheDecision::Rehash(RehashReason::FingerprintChanged)
            );
        }
    }

    #[test]
    fn matching_quiet_row_is_hit() {
        let observed = fingerprint();
        let mut session = session_with_rows([row("app/main.rs", observed)], 1, &KEY);

        assert_eq!(
            session.decide("app/main.rs", &observed),
            CacheDecision::Hit {
                content_id: ContentId::new("cid_cached"),
                byte_len: 12,
            }
        );
    }

    #[test]
    fn root_level_only_deletes_unobserved_root_row_but_leaves_deep_rows() {
        let mut session = session_with_rows(
            [
                row("README.md", fingerprint()),
                row("stale-root.txt", fingerprint()),
                row("app/src/main.rs", fingerprint()),
            ],
            1,
            &KEY,
        );

        // Only README.md was observed by the shallow root pass.
        let observed = BTreeSet::from(["README.md".to_string()]);
        let write_back =
            session.finish_with_delete_scope(&observed, StatCacheDeleteScope::RootLevelOnly);

        // The unobserved root-level row is pruned; the deep row is preserved
        // because the shallow pass never descended into `app/`.
        assert_eq!(
            write_back.deletes,
            BTreeSet::from(["stale-root.txt".to_string()])
        );
    }

    #[test]
    fn root_level_only_never_deletes_any_nested_row() {
        let mut session = session_with_rows(
            [
                row("a/b.rs", fingerprint()),
                row("deep/nested/c.rs", fingerprint()),
            ],
            1,
            &KEY,
        );

        // Nothing observed at all — a `RootLevelOnly` scope must still never
        // touch a `contains('/')` row.
        let write_back =
            session.finish_with_delete_scope(&BTreeSet::new(), StatCacheDeleteScope::RootLevelOnly);

        assert!(write_back.deletes.is_empty());
    }

    #[test]
    fn under_roots_alone_never_deletes_unrelated_root_level_rows() {
        let mut session = session_with_rows(
            [
                row("keep-root.txt", fingerprint()),
                row("src/gone.rs", fingerprint()),
            ],
            1,
            &KEY,
        );

        let roots = BTreeSet::from(["src".to_string()]);
        // Only the scoped subtree was scanned; `keep-root.txt` was not observed
        // but is outside `src`, so it must survive.
        let write_back = session
            .finish_with_delete_scope(&BTreeSet::new(), StatCacheDeleteScope::UnderRoots(&roots));

        assert_eq!(
            write_back.deletes,
            BTreeSet::from(["src/gone.rs".to_string()])
        );
    }

    #[test]
    fn under_roots_and_root_level_prunes_root_and_scoped_but_preserves_unrelated_deep() {
        let mut session = session_with_rows(
            [
                row("gone-root.txt", fingerprint()),
                row("src/gone.rs", fingerprint()),
                row("other/keep.rs", fingerprint()),
            ],
            1,
            &KEY,
        );

        let roots = BTreeSet::from(["src".to_string()]);
        // Combined tick observed nothing this round; the delete scope owns the
        // root level and everything under `src`, but not the unrelated `other/`.
        let write_back = session.finish_with_delete_scope(
            &BTreeSet::new(),
            StatCacheDeleteScope::UnderRootsAndRootLevel(&roots),
        );

        assert_eq!(
            write_back.deletes,
            BTreeSet::from(["gone-root.txt".to_string(), "src/gone.rs".to_string()])
        );
    }

    #[test]
    fn live_observed_predicate_matches_each_scope() {
        use crate::sync::FullScanReason;

        // Full observes everything.
        let full = ScanScope::Full(FullScanReason::CliRequested);
        assert!(path_is_live_observed("README.md", &full));
        assert!(path_is_live_observed("deep/nested/x.rs", &full));

        // RootShallow observes only root-level entries.
        assert!(path_is_live_observed("README.md", &ScanScope::RootShallow));
        assert!(!path_is_live_observed(
            "app/main.rs",
            &ScanScope::RootShallow
        ));

        let roots = BTreeSet::from(["src".to_string()]);
        // Subtree-only observes under the roots; root-level and unrelated deep are
        // preserved (not live-observed).
        let subtree = ScanScope::DirtySubtrees {
            roots: roots.clone(),
            root_shallow: false,
        };
        assert!(path_is_live_observed("src/app.rs", &subtree));
        assert!(path_is_live_observed("src", &subtree));
        assert!(!path_is_live_observed("README.md", &subtree));
        assert!(!path_is_live_observed("other/deep.rs", &subtree));

        // Combined observes under the roots AND root-level; only deep-outside-root
        // entries are preserved.
        let combined = ScanScope::DirtySubtrees {
            roots,
            root_shallow: true,
        };
        assert!(path_is_live_observed("src/app.rs", &combined));
        assert!(path_is_live_observed("README.md", &combined));
        assert!(!path_is_live_observed("other/deep.rs", &combined));
    }

    fn session_with_rows<const N: usize>(
        rows: [StatCacheRow; N],
        key_epoch: u32,
        content_key: &[u8; 32],
    ) -> StatCacheSession {
        StatCacheSession {
            rows: rows
                .into_iter()
                .map(|row| (row.path.clone(), row))
                .collect(),
            scan_started_ns: FileTimestampNanos::new(100),
            key_epoch,
            content_key_fingerprint: ContentKeyFingerprint::from_content_key(content_key),
            upserts: Vec::new(),
            stats: ScanStats::default(),
        }
    }

    fn row(path: &str, fingerprint: StatFingerprint) -> StatCacheRow {
        StatCacheRow {
            path: path.to_string(),
            fingerprint,
            key_epoch: 1,
            content_key_fingerprint: ContentKeyFingerprint::from_content_key(&KEY),
            content_id: ContentId::new("cid_cached"),
            byte_len: 12,
            format_version: STAT_CACHE_FORMAT_VERSION,
            hashed_at_ns: FileTimestampNanos::new(50),
            last_verified_at: "2026-07-04T00:00:00Z".to_string(),
        }
    }

    fn fingerprint() -> StatFingerprint {
        StatFingerprint {
            size: 12,
            mtime_ns: FileTimestampNanos::new(10),
            ctime_ns: FileTimestampNanos::new(11),
            inode: 12,
            dev: 13,
            file_mode: 0o100644,
        }
    }
}
