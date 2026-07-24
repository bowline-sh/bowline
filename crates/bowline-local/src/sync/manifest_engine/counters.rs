//! In-memory operational counters for the manifest engine (Plan 111 Step 5).
//!
//! These are the honest cost meters the release gate reads to enforce the
//! Plan 108 C1–C5 budgets: idle does no work (C1), an edit costs the edit (C2),
//! a stat-walk hashes nothing (C5), and so on. Every counter is a monotonic
//! `AtomicU64` incremented at the one site that performs the work, so a status
//! consumer on another thread reads a consistent, lock-free tally.
//!
//! The counters never claim a number they cannot back: a value is bumped only
//! where the work actually happens (a real PUT, a committed write transaction, a
//! content open), never inferred from a coarser outcome.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The shared, lock-free engine cost meters. Held as an `Arc` by the engine and
/// cloned into the driver's status handle so the daemon can read it concurrently
/// with the engine thread that writes it.
#[derive(Debug, Default)]
pub struct EngineCounters {
    /// Full stat-walk passes performed (the C5 safety audit and startup seed).
    pub stat_walks: AtomicU64,
    /// Paths stat-ed across all walks (the C5 "10k stat-walk" budget subject).
    pub stat_entries: AtomicU64,
    /// Files opened and read for content (the C1/C5 "zero content opens" meter).
    pub content_opens: AtomicU64,
    /// Content identities computed (BLAKE3 over plaintext).
    pub content_hashes: AtomicU64,
    /// Plaintext bytes fed through hashing.
    pub hashed_bytes: AtomicU64,
    /// Write transactions committed against the ancestor store (the C1 "zero
    /// SQLite mutation when idle" meter).
    pub sqlite_mutations: AtomicU64,
    /// Blob objects PUT to the object store (skips on dedup do not count).
    pub blob_uploads: AtomicU64,
    /// Manifest objects PUT to the object store.
    pub manifest_uploads: AtomicU64,
    /// Ref compare-and-swap attempts.
    pub cas_attempts: AtomicU64,
    /// Ref compare-and-swap losses (a normal, non-attention outcome).
    pub cas_losses: AtomicU64,
    /// Retries: backoff re-arms plus in-cycle push retries after a lost CAS.
    pub retries: AtomicU64,
    /// Filesystem apply operations (installs, deletes, mode changes, asides).
    pub apply_ops: AtomicU64,
    /// Dirty paths a push could not settle because they were being actively
    /// written (twice-diverged) and were retained for a later rescan. Observable
    /// so a churning path that keeps being deferred is visible, distinct from a
    /// lost-CAS retry.
    pub push_skips: AtomicU64,
}

impl EngineCounters {
    /// A fresh shared counter set. Every construction path (engine, tests) uses
    /// this so there is a single owner of the initial state.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_stat_walk(&self, entries: u64, hashes: u64) {
        self.stat_walks.fetch_add(1, Ordering::Relaxed);
        self.stat_entries.fetch_add(entries, Ordering::Relaxed);
        self.content_hashes.fetch_add(hashes, Ordering::Relaxed);
    }

    pub fn record_content_open(&self, bytes: u64) {
        self.content_opens.fetch_add(1, Ordering::Relaxed);
        self.hashed_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_content_hash(&self) {
        self.content_hashes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_sqlite_mutation(&self) {
        self.sqlite_mutations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_blob_upload(&self) {
        self.blob_uploads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_manifest_upload(&self) {
        self.manifest_uploads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cas_attempt(&self) {
        self.cas_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cas_loss(&self) {
        self.cas_losses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_apply_ops(&self, ops: u64) {
        self.apply_ops.fetch_add(ops, Ordering::Relaxed);
    }

    pub fn record_push_skip(&self, paths: u64) {
        self.push_skips.fetch_add(paths, Ordering::Relaxed);
    }

    /// A plain-value copy for crossing the thread boundary into the daemon
    /// status/metrics surface (mirrors [`ManifestEngine::snapshot`]).
    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            stat_walks: self.stat_walks.load(Ordering::Relaxed),
            stat_entries: self.stat_entries.load(Ordering::Relaxed),
            content_opens: self.content_opens.load(Ordering::Relaxed),
            content_hashes: self.content_hashes.load(Ordering::Relaxed),
            hashed_bytes: self.hashed_bytes.load(Ordering::Relaxed),
            sqlite_mutations: self.sqlite_mutations.load(Ordering::Relaxed),
            blob_uploads: self.blob_uploads.load(Ordering::Relaxed),
            manifest_uploads: self.manifest_uploads.load(Ordering::Relaxed),
            cas_attempts: self.cas_attempts.load(Ordering::Relaxed),
            cas_losses: self.cas_losses.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            apply_ops: self.apply_ops.load(Ordering::Relaxed),
            push_skips: self.push_skips.load(Ordering::Relaxed),
        }
    }
}

/// An owned, point-in-time copy of the counters. Serialized field-order-stable
/// through the daemon metrics RPC; the release gate reads these exact names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CountersSnapshot {
    pub stat_walks: u64,
    pub stat_entries: u64,
    pub content_opens: u64,
    pub content_hashes: u64,
    pub hashed_bytes: u64,
    pub sqlite_mutations: u64,
    pub blob_uploads: u64,
    pub manifest_uploads: u64,
    pub cas_attempts: u64,
    pub cas_losses: u64,
    pub retries: u64,
    pub apply_ops: u64,
    pub push_skips: u64,
}

impl CountersSnapshot {
    /// Stable-order JSON for the daemon `daemon.metrics` `engine` key. Hand-free
    /// via `serde_json::json!` (house rule: never `format!` JSON).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "statWalks": self.stat_walks,
            "statEntries": self.stat_entries,
            "contentOpens": self.content_opens,
            "contentHashes": self.content_hashes,
            "hashedBytes": self.hashed_bytes,
            "sqliteMutations": self.sqlite_mutations,
            "blobUploads": self.blob_uploads,
            "manifestUploads": self.manifest_uploads,
            "casAttempts": self.cas_attempts,
            "casLosses": self.cas_losses,
            "retries": self.retries,
            "applyOps": self.apply_ops,
            "pushSkips": self.push_skips,
        })
    }
}
