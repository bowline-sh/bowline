//! The manifest-sync engine's own SQLite store (Plan 109 Step 2).
//!
//! This owns `manifest_engine.sqlite3` — a database distinct from the product's
//! `local.sqlite3`, which is never touched here. `files` is the three-way
//! ancestor (state at last successful sync) carrying both portable identity and
//! the local stat fingerprint; `engine_state` is a typed singleton row (never a
//! key/value registry); `intents` is the crash-recovery journal for in-flight
//! apply operations; `unsyncable` records paths this device cannot sync; and
//! `blobs`/`tree_nodes` are the two content-addressed ledgers of objects this
//! device has proven the store already holds.
//!
//! Portable `files` identity mutates only with a proven push/pull or intent
//! recovery. [`ManifestStore::refresh_local_file_records`] may separately
//! refresh local-only stat observations after byte verification without
//! changing the manifest identity.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bowline_core::ids::ContentId;
use rusqlite::{Connection, OptionalExtension, params};

use super::endpoint::EndpointInstant;
use super::manifest::directory_tree::SubtreeHash;
use super::manifest::{BlobKey, EntryKind, FileMode, KeyEpoch, ManifestKey, WorkspacePath};
use super::unsyncable::{UnsyncableReason, UnsyncableRecord};

#[path = "store/error.rs"]
mod error;
#[path = "store/file_queries.rs"]
mod file_queries;
#[path = "store/frontier.rs"]
mod frontier;
#[path = "store/schema.rs"]
mod schema;
pub use error::ManifestStoreError;
pub use frontier::{EngineState, MaterializationRevision};
use schema::SCHEMA;

const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);

/// The local stat fingerprint used for cheap change detection. Never crosses
/// the wire; the changing bits (mtime/ctime/inode/dev) live here while
/// kind/size/mode live on [`FileRecord`] directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFingerprint {
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub dev: u64,
}

/// One ancestor row: portable identity plus the local fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub kind: EntryKind,
    pub size: u64,
    pub mode: FileMode,
    pub symlink_target: Option<String>,
    pub content_id: Option<ContentId>,
    pub blob_key: Option<BlobKey>,
    pub key_epoch: Option<KeyEpoch>,
    pub fingerprint: StatFingerprint,
    pub hashed_at: Option<i64>,
    /// The endpoint-clock reading this row was PROVED against, or `None` when
    /// the cycle that wrote it could prove nothing. Never a wall-clock instant:
    /// see [`super::endpoint`] for why the two clocks disagree and what that
    /// disagreement costs. Written only by `endpoint::prove_rows`.
    pub verified_at: Option<EndpointInstant>,
}

/// One row of the blob ledger: content this device has already sealed and PUT.
///
/// The ledger is what turns content addressing into actual dedup. A rename, an
/// A→B→A edit cycle, and a `git checkout` that returns to a previous branch all
/// re-present content whose sealed object is already in the store; without this
/// row the push re-seals it (zstd + AEAD over the whole file) and pays another
/// create-only PUT round trip to write bytes that are already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedBlob {
    pub blob_key: BlobKey,
    pub key_epoch: KeyEpoch,
    pub byte_len: u64,
}

/// A change-proportional ancestor mutation: upsert these rows, remove these
/// paths. Never a whole-table rewrite — an edit costs the edit (invariant C2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AncestorCommit {
    pub upserts: BTreeMap<WorkspacePath, FileRecord>,
    pub removals: BTreeSet<WorkspacePath>,
}

/// The apply operation an intent journals. Step 5 (pull/apply) owns the full
/// semantics and may extend this set; the store only round-trips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentOperationKind {
    Install,
    Delete,
    ModeChange,
    ConflictAside,
}

impl IntentOperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Delete => "delete",
            Self::ModeChange => "mode-change",
            Self::ConflictAside => "conflict-aside",
        }
    }

    fn from_str(value: &str) -> Result<Self, ManifestStoreError> {
        match value {
            "install" => Ok(Self::Install),
            "delete" => Ok(Self::Delete),
            "mode-change" => Ok(Self::ModeChange),
            "conflict-aside" => Ok(Self::ConflictAside),
            _ => Err(ManifestStoreError::Corrupt {
                field: "operation_kind",
            }),
        }
    }
}

/// A journaled apply intent. The `expected_preimage`, `target_record`, and
/// `preserved_preimage` columns are opaque serialized payloads authored and
/// interpreted by pull/apply (Step 5); the store persists them verbatim inside
/// the atomic outcome and never parses them, keeping their schema owned by the
/// single domain consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub path: WorkspacePath,
    pub operation_kind: IntentOperationKind,
    pub temp_name: Option<String>,
    pub expected_preimage: Option<String>,
    pub target_record: Option<String>,
    pub preserved_preimage: Option<String>,
    pub target_manifest_key: Option<ManifestKey>,
    pub created_at: i64,
}

pub struct ManifestStore {
    connection: Connection,
}

impl ManifestStore {
    /// Opens (creating if needed) the engine store at `path`, e.g.
    /// `<state root>/manifest_engine.sqlite3`. The caller owns creation of the
    /// parent state root — the engine never mutates the filesystem outside its
    /// own database file, so directory provisioning belongs to the daemon that
    /// already establishes the state root.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ManifestStoreError> {
        let path = path.as_ref();
        let connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // WAL is durable database state; establish it and the schema once.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        // Exact convergence receipts bind to the latest committed applied ref
        // and materialization revision. FULL makes a successful WAL commit
        // survive power loss; NORMAL explicitly permits losing the last
        // transaction and therefore cannot underwrite that receipt.
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(SCHEMA)?;
        ensure_materialization_revision(&connection)?;
        raise_ratchet_floor(&connection)?;
        Ok(Self { connection })
    }

    /// Opens the engine store read-only for diagnostics (`bowline doctor`).
    /// Unlike [`ManifestStore::open`] this never creates the file and never
    /// mutates schema or WAL — a read-only probe must not write. Fails if the
    /// database is absent, so the caller reports a missing engine truthfully.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, ManifestStoreError> {
        let connection =
            Connection::open_with_flags(path.as_ref(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Self { connection })
    }

    /// Runs `PRAGMA quick_check` and reports whether SQLite considers the
    /// database structurally intact. The pragma yields the single row `ok` on
    /// success; any other text is a corruption signal. Read-only.
    pub fn quick_check(&self) -> Result<bool, ManifestStoreError> {
        let verdict: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))?;
        Ok(verdict == "ok")
    }

    /// Every path the engine currently cannot sync, sorted by path.
    pub fn unsyncable(
        &self,
    ) -> Result<BTreeMap<WorkspacePath, UnsyncableRecord>, ManifestStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT path, reason, errno, observed_at FROM unsyncable ORDER BY path")?;
        let rows = statement.query_map([], |row| Ok(row_to_unsyncable(row)))?;
        let mut entries = BTreeMap::new();
        for row in rows {
            let (path, record) = row??;
            entries.insert(path, record);
        }
        Ok(entries)
    }

    /// Replace the unsyncable rows for the paths a scan just examined: `observed`
    /// are the paths that failed, `resolved` the paths that were examined and
    /// succeeded (so a fixed permission clears its attention item). One
    /// transaction, so status never sees a half-updated set.
    pub fn record_unsyncable(
        &mut self,
        observed: &BTreeMap<WorkspacePath, UnsyncableRecord>,
        resolved: &BTreeSet<WorkspacePath>,
    ) -> Result<(), ManifestStoreError> {
        if observed.is_empty() && resolved.is_empty() {
            return Ok(());
        }
        self.in_transaction(|connection| {
            {
                let mut clear = connection.prepare("DELETE FROM unsyncable WHERE path = ?1")?;
                for path in resolved {
                    clear.execute(params![path.as_str()])?;
                }
            }
            let mut upsert = connection.prepare(
                "INSERT INTO unsyncable (path, reason, errno, observed_at) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT(path) DO UPDATE SET \
                 reason = excluded.reason, errno = excluded.errno, \
                 observed_at = excluded.observed_at",
            )?;
            for (path, record) in observed {
                upsert.execute(params![
                    path.as_str(),
                    record.reason.tag(),
                    record.errno,
                    record.observed_at,
                ])?;
            }
            Ok(())
        })
    }

    /// The sealed object this device already holds for `content_id` under the
    /// current key epoch, if any. A primary-key lookup, so the ledger costs
    /// O(log rows) per changed file and never a table scan.
    ///
    /// Epoch-scoped deliberately: a key rotation must re-seal, so a row from an
    /// older epoch is not a hit.
    pub fn sealed_blob(
        &self,
        content_id: &ContentId,
        key_epoch: KeyEpoch,
    ) -> Result<Option<SealedBlob>, ManifestStoreError> {
        self.connection
            .query_row(
                "SELECT blob_key, key_epoch, byte_len FROM blobs WHERE content_id = ?1 \
                 AND key_epoch = ?2",
                params![content_id.as_str(), i64::from(key_epoch.get())],
                |row| Ok(row_to_sealed_blob(row)),
            )
            .optional()?
            .transpose()
    }

    /// Record content this push actually sealed and PUT, so a later push that
    /// re-presents the same bytes skips both.
    pub fn record_sealed_blobs(
        &mut self,
        blobs: &BTreeMap<ContentId, SealedBlob>,
    ) -> Result<(), ManifestStoreError> {
        if blobs.is_empty() {
            return Ok(());
        }
        self.in_transaction(|connection| {
            let mut upsert = connection.prepare(
                "INSERT INTO blobs (content_id, blob_key, key_epoch, byte_len) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT(content_id) DO UPDATE SET \
                 blob_key = excluded.blob_key, key_epoch = excluded.key_epoch, \
                 byte_len = excluded.byte_len",
            )?;
            for (content_id, blob) in blobs {
                upsert.execute(params![
                    content_id.as_str(),
                    blob.blob_key.as_str(),
                    i64::from(blob.key_epoch.get()),
                    to_i64(blob.byte_len)?,
                ])?;
            }
            Ok(())
        })
    }

    /// The node object this device has proven exists for a subtree with this
    /// content, if any. A primary-key lookup, so a publish costs O(log rows) per
    /// directory it touches and never a scan.
    ///
    /// Keyed by CONTENT, never by directory path: a row therefore cannot go
    /// stale as the ancestor moves, and no transaction has to keep this table in
    /// step with `files`. The worst a crash can do is lose a cached row, costing
    /// one reseal. Epoch-scoped for the same reason the blob ledger is — a key
    /// rotation must re-seal, so an older epoch's row is not a hit.
    pub fn tree_node(
        &self,
        subtree_hash: &SubtreeHash,
        key_epoch: KeyEpoch,
    ) -> Result<Option<ManifestKey>, ManifestStoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT node_key FROM tree_nodes WHERE subtree_hash = ?1 AND key_epoch = ?2",
                params![subtree_hash.as_str(), i64::from(key_epoch.get())],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(ManifestKey::new))
    }

    /// Record subtrees whose node object this device has proven present — it
    /// either uploaded it or just downloaded it. Never called for a node whose
    /// upload did not return.
    pub fn record_tree_nodes(
        &mut self,
        nodes: &BTreeMap<SubtreeHash, ManifestKey>,
        key_epoch: KeyEpoch,
    ) -> Result<(), ManifestStoreError> {
        if nodes.is_empty() {
            return Ok(());
        }
        self.in_transaction(|connection| {
            let mut upsert = connection.prepare(
                "INSERT INTO tree_nodes (subtree_hash, node_key, key_epoch) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(subtree_hash) DO UPDATE SET \
                 node_key = excluded.node_key, key_epoch = excluded.key_epoch",
            )?;
            for (subtree_hash, node_key) in nodes {
                upsert.execute(params![
                    subtree_hash.as_str(),
                    node_key.as_str(),
                    i64::from(key_epoch.get()),
                ])?;
            }
            record_tree_node_epochs(connection, nodes.values(), key_epoch)?;
            Ok(())
        })
    }

    /// Record the epoch of a copy-on-write root whose upload returned. The
    /// subtree-hash ledger has no flat manifest from which to derive hashes on
    /// this path, but the physical-key epoch proof is independently sufficient
    /// to distinguish an ordinary edit from a key-rotation rebuild.
    pub fn record_tree_node_epoch(
        &mut self,
        node_key: &ManifestKey,
        key_epoch: KeyEpoch,
    ) -> Result<(), ManifestStoreError> {
        self.in_transaction(|connection| {
            record_tree_node_epochs(connection, std::iter::once(node_key), key_epoch)
        })
    }

    /// The singleton engine-state row (default if not yet written).
    pub fn engine_state(&self) -> Result<EngineState, ManifestStoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT applied_manifest_key, last_ref_version, materialization_revision, \
                 highest_verified_ref_version, highest_verified_manifest_key \
                 FROM engine_state WHERE singleton = 1",
                [],
                |row| Ok(row_to_engine_state(row)),
            )
            .optional()?;
        match state {
            Some(state) => state,
            None => Ok(EngineState::default()),
        }
    }

    /// Durable ref-freshness ratchet. Advances the highest verified ref version
    /// and manifest key (monotonic-max, see [`advance_verified_ratchet`]) without
    /// disturbing the applied state.
    pub fn record_highest_verified(
        &mut self,
        ref_version: u64,
        manifest_key: &ManifestKey,
    ) -> Result<(), ManifestStoreError> {
        let version = to_i64(ref_version)?;
        self.in_transaction(|connection| {
            advance_verified_ratchet(connection, manifest_key, version)
        })
    }

    /// An already-applied manifest key re-observed at a NEWER ref version (an
    /// A→B→A hosted sequence while this device was offline). The content is the
    /// verified manifest this device already holds, so both the applied version
    /// and the freshness ratchet advance in one transaction — without this,
    /// every subsequent push would CAS against the stale stored version and
    /// livelock on an already-current pull.
    pub fn record_ref_advance(
        &mut self,
        manifest_key: &ManifestKey,
        ref_version: u64,
    ) -> Result<(), ManifestStoreError> {
        let version = to_i64(ref_version)?;
        self.in_transaction(|connection| {
            set_applied(connection, manifest_key, version)?;
            advance_verified_ratchet(connection, manifest_key, version)
        })
    }

    /// Atomically commit the pushed ancestor and its verified ref.
    pub fn commit_push_success(
        &mut self,
        commit: &AncestorCommit,
        manifest_key: &ManifestKey,
        ref_version: u64,
        settled_paths: &BTreeSet<WorkspacePath>,
    ) -> Result<(), ManifestStoreError> {
        let version = to_i64(ref_version)?;
        self.in_transaction(|connection| {
            apply_ancestor(connection, commit)?;
            set_applied(connection, manifest_key, version)?;
            // A successful CAS to `version` is a verified observation of the hosted
            // head this device just published. Advance the freshness ratchet in the
            // SAME transaction so a later hosted rollback to a lower version cannot
            // pass `enforce_freshness` and revert workspace state we published — the
            // gap when no pull has ever populated the ratchet.
            advance_verified_ratchet(connection, manifest_key, version)?;
            delete_pending_push_paths(connection, settled_paths)?;
            Ok(())
        })
    }

    pub fn pending_push_paths(&self) -> Result<BTreeSet<WorkspacePath>, ManifestStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT path FROM pending_push ORDER BY path")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut paths = BTreeSet::new();
        for row in rows {
            paths.insert(WorkspacePath::new(row?));
        }
        Ok(paths)
    }

    pub fn clear_pending_push_paths(
        &mut self,
        paths: &BTreeSet<WorkspacePath>,
    ) -> Result<(), ManifestStoreError> {
        self.in_transaction(|connection| delete_pending_push_paths(connection, paths))
    }

    /// Atomically settle a push that changed no remote tree while retaining
    /// verified local-only stat observations. A crash cannot clear the pending
    /// path while leaving its old fingerprint behind.
    pub fn commit_push_no_change(
        &mut self,
        commit: &AncestorCommit,
        settled_paths: &BTreeSet<WorkspacePath>,
    ) -> Result<(), ManifestStoreError> {
        self.in_transaction(|connection| {
            apply_ancestor(connection, commit)?;
            delete_pending_push_paths(connection, settled_paths)
        })
    }

    /// Refresh local-only observations after the caller verified that the file
    /// bytes and portable manifest identity are unchanged. This never advances
    /// the applied ref or alters hosted identity.
    pub fn refresh_local_file_records(
        &mut self,
        records: &BTreeMap<WorkspacePath, FileRecord>,
    ) -> Result<(), ManifestStoreError> {
        let commit = AncestorCommit {
            upserts: records.clone(),
            removals: BTreeSet::new(),
        };
        self.in_transaction(|connection| apply_ancestor(connection, &commit))
    }

    /// One transaction commits the pulled ancestor rows, optionally advances the
    /// applied ref, optionally advances the freshness ratchet, and deletes the
    /// listed intents. Intents die only here — there is no standalone clear step
    /// (Plan 109 review Change 2). `applied` is `None` when the pull deferred
    /// content it has not materialized (an active Git lock with no prior head): the
    /// ancestor rows for what WAS applied still commit, but the applied head is held
    /// back so `already_current` cannot short-circuit the retry that must finish the
    /// deferred paths. `verified` is the head the pull actually fetched,
    /// authenticated, and decoded; it advances the ratchet HERE — never before the
    /// manifest is verified — so a missing/corrupt object or a forged high-version
    /// ref cannot freeze the ratchet with nothing verified. It is `Some` even when
    /// `applied` is held back (a deferred head was still authenticated), and `None`
    /// for crash recovery, which re-derives the true head on the follow-on pull.
    pub fn commit_pull_outcome(
        &mut self,
        commit: &AncestorCommit,
        applied: Option<(&ManifestKey, u64)>,
        verified: Option<(&ManifestKey, u64)>,
        intent_ids: &[WorkspacePath],
        push_again: &BTreeSet<WorkspacePath>,
    ) -> Result<(), ManifestStoreError> {
        let materialization_changed =
            !commit.upserts.is_empty() || !commit.removals.is_empty() || !intent_ids.is_empty();
        self.in_transaction(|connection| {
            apply_ancestor(connection, commit)?;
            if let Some((manifest_key, ref_version)) = applied {
                set_applied(connection, manifest_key, to_i64(ref_version)?)?;
            } else if materialization_changed {
                advance_materialization_revision(connection)?;
            }
            if let Some((manifest_key, ref_version)) = verified {
                advance_verified_ratchet(connection, manifest_key, to_i64(ref_version)?)?;
            }
            let mut delete = connection.prepare("DELETE FROM intents WHERE path = ?1")?;
            for path in intent_ids {
                delete.execute(params![path.as_str()])?;
            }
            let mut insert =
                connection.prepare("INSERT OR IGNORE INTO pending_push(path) VALUES (?1)")?;
            for path in push_again {
                insert.execute(params![path.as_str()])?;
            }
            Ok(())
        })
    }

    /// Records (upserts by path) an apply intent before its filesystem mutation.
    pub fn open_intent(&mut self, intent: &Intent) -> Result<(), ManifestStoreError> {
        self.connection.execute(
            "INSERT INTO intents (path, operation_kind, temp_name, expected_preimage, \
             target_record, preserved_preimage, target_manifest_key, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(path) DO UPDATE SET \
             operation_kind = excluded.operation_kind, temp_name = excluded.temp_name, \
             expected_preimage = excluded.expected_preimage, target_record = excluded.target_record, \
             preserved_preimage = excluded.preserved_preimage, \
             target_manifest_key = excluded.target_manifest_key, created_at = excluded.created_at",
            params![
                intent.path.as_str(),
                intent.operation_kind.as_str(),
                intent.temp_name,
                intent.expected_preimage,
                intent.target_record,
                intent.preserved_preimage,
                intent.target_manifest_key.as_ref().map(ManifestKey::as_str),
                intent.created_at,
            ],
        )?;
        Ok(())
    }

    /// All journaled intents, sorted by path for deterministic recovery.
    pub fn pending_intents(&self) -> Result<Vec<Intent>, ManifestStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT path, operation_kind, temp_name, expected_preimage, target_record, \
             preserved_preimage, target_manifest_key, created_at FROM intents ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| Ok(row_to_intent(row)))?;
        let mut intents = Vec::new();
        for row in rows {
            intents.push(row??);
        }
        Ok(intents)
    }

    // BEGIN IMMEDIATE .. COMMIT with rollback on any error, so a partial
    // outcome is never observable. Private, but reachable from the child test
    // module to exercise the rollback path directly.
    fn in_transaction<T>(
        &self,
        body: impl FnOnce(&Connection) -> Result<T, ManifestStoreError>,
    ) -> Result<T, ManifestStoreError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        match body(&self.connection) {
            Ok(value) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

fn delete_pending_push_paths(
    connection: &Connection,
    paths: &BTreeSet<WorkspacePath>,
) -> Result<(), ManifestStoreError> {
    let mut delete = connection.prepare("DELETE FROM pending_push WHERE path = ?1")?;
    for path in paths {
        delete.execute(params![path.as_str()])?;
    }
    Ok(())
}

fn apply_ancestor(
    connection: &Connection,
    commit: &AncestorCommit,
) -> Result<(), ManifestStoreError> {
    {
        let mut remove = connection.prepare("DELETE FROM files WHERE path = ?1")?;
        for path in &commit.removals {
            remove.execute(params![path.as_str()])?;
        }
    }
    let mut upsert = connection.prepare(
        "INSERT INTO files (path, kind, size, mode, symlink_target, content_id, blob_key, \
         key_epoch, mtime_ns, ctime_ns, inode, dev, hashed_at, verified_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT(path) DO UPDATE SET \
         kind = excluded.kind, size = excluded.size, mode = excluded.mode, \
         symlink_target = excluded.symlink_target, content_id = excluded.content_id, \
         blob_key = excluded.blob_key, key_epoch = excluded.key_epoch, \
         mtime_ns = excluded.mtime_ns, ctime_ns = excluded.ctime_ns, inode = excluded.inode, \
         dev = excluded.dev, hashed_at = excluded.hashed_at, verified_at = excluded.verified_at",
    )?;
    for (path, record) in &commit.upserts {
        upsert.execute(params![
            path.as_str(),
            entry_kind_to_i64(record.kind),
            to_i64(record.size)?,
            i64::from(record.mode.get()),
            record.symlink_target,
            record.content_id.as_ref().map(ContentId::as_str),
            record.blob_key.as_ref().map(BlobKey::as_str),
            record.key_epoch.map(|epoch| i64::from(epoch.get())),
            record.fingerprint.mtime_ns,
            record.fingerprint.ctime_ns,
            record.fingerprint.inode as i64,
            record.fingerprint.dev as i64,
            record.hashed_at,
            record.verified_at.map(EndpointInstant::nanos),
        ])?;
    }
    Ok(())
}

/// Re-derive the ref-freshness ratchet floor from the applied head on every
/// open.
///
/// `synchronous=NORMAL` trades a lost *last* transaction on power loss for the
/// fsync cost of every commit. Everywhere else that trade converges — the next
/// cycle re-observes disk and the hosted ref and redoes the lost work — but the
/// ratchet is the one durable value whose whole job is to be monotone. A window
/// where the applied head sits above the ratchet is a window where a hosted
/// rollback below the head this device already applied would pass
/// `enforce_freshness`. The applied head is itself a verified observation (it was
/// either CAS-advanced by this device or fetched, authenticated, and decoded), so
/// raising the floor to it costs nothing and closes the window. Never lowers:
/// the `WHERE` clause is the same monotone guard as
/// [`advance_verified_ratchet`].
fn raise_ratchet_floor(connection: &Connection) -> Result<(), ManifestStoreError> {
    connection.execute(
        "UPDATE engine_state SET \
         highest_verified_ref_version = last_ref_version, \
         highest_verified_manifest_key = applied_manifest_key \
         WHERE singleton = 1 AND last_ref_version IS NOT NULL \
         AND applied_manifest_key IS NOT NULL \
         AND (highest_verified_ref_version IS NULL \
         OR highest_verified_ref_version < last_ref_version)",
        [],
    )?;
    Ok(())
}

fn set_applied(
    connection: &Connection,
    manifest_key: &ManifestKey,
    ref_version: i64,
) -> Result<(), ManifestStoreError> {
    let next = next_materialization_revision(connection)?;
    connection.execute(
        "INSERT INTO engine_state (singleton, applied_manifest_key, last_ref_version, \
         materialization_revision) VALUES (1, ?1, ?2, ?3) \
         ON CONFLICT(singleton) DO UPDATE SET \
         applied_manifest_key = excluded.applied_manifest_key, \
         last_ref_version = excluded.last_ref_version, \
         materialization_revision = excluded.materialization_revision",
        params![manifest_key.as_str(), ref_version, next],
    )?;
    Ok(())
}

fn advance_materialization_revision(connection: &Connection) -> Result<(), ManifestStoreError> {
    let next = next_materialization_revision(connection)?;
    connection.execute(
        "INSERT INTO engine_state (singleton, materialization_revision) VALUES (1, ?1) \
         ON CONFLICT(singleton) DO UPDATE SET \
         materialization_revision = excluded.materialization_revision",
        [next],
    )?;
    Ok(())
}

fn next_materialization_revision(connection: &Connection) -> Result<i64, ManifestStoreError> {
    connection
        .query_row(
            "SELECT materialization_revision FROM engine_state WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(ManifestStoreError::ValueOutOfRange {
            field: "materialization_revision",
        })
}

/// Add the durable materialization frontier to stores created by a preceding
/// development build. Bowline is greenfield, so this is a one-way schema
/// upgrade rather than a runtime compatibility shape.
fn ensure_materialization_revision(connection: &Connection) -> Result<(), ManifestStoreError> {
    let mut columns = connection.prepare("PRAGMA table_info(engine_state)")?;
    let names = columns.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == "materialization_revision" {
            return Ok(());
        }
    }
    connection.execute(
        "ALTER TABLE engine_state ADD COLUMN materialization_revision INTEGER NOT NULL DEFAULT 0",
        [],
    )?;
    Ok(())
}

fn record_tree_node_epochs<'a>(
    connection: &Connection,
    node_keys: impl IntoIterator<Item = &'a ManifestKey>,
    key_epoch: KeyEpoch,
) -> Result<(), ManifestStoreError> {
    let mut upsert = connection.prepare(
        "INSERT INTO tree_node_epochs (node_key, key_epoch) VALUES (?1, ?2) \
         ON CONFLICT(node_key) DO UPDATE SET key_epoch = excluded.key_epoch",
    )?;
    for node_key in node_keys {
        upsert.execute(params![node_key.as_str(), i64::from(key_epoch.get())])?;
    }
    Ok(())
}

/// The SINGLE writer of the ref-freshness ratchet
/// (`highest_verified_ref_version` + `highest_verified_manifest_key`). Shared by
/// the push-success and pull-outcome transactions so "verified" means one thing on
/// both paths: a push CAS to version N and a pull that fetched+authenticated the
/// head at version N are equally genuine observations of the hosted head.
///
/// INVARIANT (monotonic-max): the ratchet only ever advances. The upsert updates
/// the row only when the incoming version exceeds the stored one (or none is
/// stored yet), so an echo, genesis, or out-of-order observation is a no-op and can
/// never lower it. That monotonicity is exactly what lets `enforce_freshness`
/// detect a hosted rollback to a lower version as a regression rather than silently
/// apply it.
fn advance_verified_ratchet(
    connection: &Connection,
    manifest_key: &ManifestKey,
    ref_version: i64,
) -> Result<(), ManifestStoreError> {
    connection.execute(
        "INSERT INTO engine_state (singleton, highest_verified_ref_version, \
         highest_verified_manifest_key) VALUES (1, ?1, ?2) \
         ON CONFLICT(singleton) DO UPDATE SET \
         highest_verified_ref_version = excluded.highest_verified_ref_version, \
         highest_verified_manifest_key = excluded.highest_verified_manifest_key \
         WHERE engine_state.highest_verified_ref_version IS NULL \
         OR excluded.highest_verified_ref_version > engine_state.highest_verified_ref_version",
        params![ref_version, manifest_key.as_str()],
    )?;
    Ok(())
}

fn row_to_file(row: &rusqlite::Row<'_>) -> Result<(WorkspacePath, FileRecord), ManifestStoreError> {
    let path = WorkspacePath::new(row.get::<_, String>(0)?);
    let record = FileRecord {
        kind: entry_kind_from_i64(row.get::<_, i64>(1)?)?,
        size: from_i64(row.get::<_, i64>(2)?)?,
        mode: FileMode::new(u32_from_i64(row.get::<_, i64>(3)?, "mode")?),
        symlink_target: row.get::<_, Option<String>>(4)?,
        content_id: row.get::<_, Option<String>>(5)?.map(ContentId::new),
        blob_key: row.get::<_, Option<String>>(6)?.map(BlobKey::new),
        key_epoch: row
            .get::<_, Option<i64>>(7)?
            .map(|value| u32_from_i64(value, "key_epoch").map(KeyEpoch::new))
            .transpose()?,
        fingerprint: StatFingerprint {
            mtime_ns: row.get::<_, i64>(8)?,
            ctime_ns: row.get::<_, i64>(9)?,
            inode: row.get::<_, i64>(10)? as u64,
            dev: row.get::<_, i64>(11)? as u64,
        },
        hashed_at: row.get::<_, Option<i64>>(12)?,
        verified_at: row
            .get::<_, Option<i64>>(13)?
            .map(EndpointInstant::from_stored_nanos),
    };
    Ok((path, record))
}

fn row_to_sealed_blob(row: &rusqlite::Row<'_>) -> Result<SealedBlob, ManifestStoreError> {
    Ok(SealedBlob {
        blob_key: BlobKey::new(row.get::<_, String>(0)?),
        key_epoch: KeyEpoch::new(u32_from_i64(row.get::<_, i64>(1)?, "key_epoch")?),
        byte_len: from_i64(row.get::<_, i64>(2)?)?,
    })
}

fn row_to_engine_state(row: &rusqlite::Row<'_>) -> Result<EngineState, ManifestStoreError> {
    let state = EngineState {
        applied_manifest_key: row.get::<_, Option<String>>(0)?.map(ManifestKey::new),
        last_ref_version: row.get::<_, Option<i64>>(1)?.map(from_i64).transpose()?,
        materialization_revision: MaterializationRevision::from_stored(from_i64(
            row.get::<_, i64>(2)?,
        )?),
        highest_verified_ref_version: row.get::<_, Option<i64>>(3)?.map(from_i64).transpose()?,
        highest_verified_manifest_key: row.get::<_, Option<String>>(4)?.map(ManifestKey::new),
    };
    let _checked = state.applied_ref()?;
    Ok(state)
}

fn row_to_intent(row: &rusqlite::Row<'_>) -> Result<Intent, ManifestStoreError> {
    Ok(Intent {
        path: WorkspacePath::new(row.get::<_, String>(0)?),
        operation_kind: IntentOperationKind::from_str(&row.get::<_, String>(1)?)?,
        temp_name: row.get::<_, Option<String>>(2)?,
        expected_preimage: row.get::<_, Option<String>>(3)?,
        target_record: row.get::<_, Option<String>>(4)?,
        preserved_preimage: row.get::<_, Option<String>>(5)?,
        target_manifest_key: row.get::<_, Option<String>>(6)?.map(ManifestKey::new),
        created_at: row.get::<_, i64>(7)?,
    })
}

fn row_to_unsyncable(
    row: &rusqlite::Row<'_>,
) -> Result<(WorkspacePath, UnsyncableRecord), ManifestStoreError> {
    let path = WorkspacePath::new(row.get::<_, String>(0)?);
    let reason = UnsyncableReason::from_tag(&row.get::<_, String>(1)?)
        .ok_or(ManifestStoreError::Corrupt { field: "reason" })?;
    Ok((
        path,
        UnsyncableRecord {
            reason,
            errno: row.get::<_, Option<i64>>(2)?.map(|code| code as i32),
            observed_at: row.get::<_, i64>(3)?,
        },
    ))
}

fn entry_kind_to_i64(kind: EntryKind) -> i64 {
    match kind {
        EntryKind::File => 0,
        EntryKind::Directory => 1,
        EntryKind::Symlink => 2,
    }
}

fn entry_kind_from_i64(value: i64) -> Result<EntryKind, ManifestStoreError> {
    match value {
        0 => Ok(EntryKind::File),
        1 => Ok(EntryKind::Directory),
        2 => Ok(EntryKind::Symlink),
        _ => Err(ManifestStoreError::Corrupt { field: "kind" }),
    }
}

fn to_i64(value: u64) -> Result<i64, ManifestStoreError> {
    i64::try_from(value).map_err(|_| ManifestStoreError::ValueOutOfRange { field: "u64->i64" })
}

fn from_i64(value: i64) -> Result<u64, ManifestStoreError> {
    u64::try_from(value).map_err(|_| ManifestStoreError::ValueOutOfRange { field: "i64->u64" })
}

fn u32_from_i64(value: i64, field: &'static str) -> Result<u32, ManifestStoreError> {
    u32::try_from(value).map_err(|_| ManifestStoreError::ValueOutOfRange { field })
}

#[cfg(test)]
#[path = "store/tests.rs"]
mod tests;
