//! The manifest-sync engine's own SQLite store (Plan 109 Step 2).
//!
//! This owns `manifest_engine.sqlite3` — a database distinct from the product's
//! `local.sqlite3`, which is never touched here. Three tables model the whole
//! engine: `files` is the three-way ancestor (state at last successful sync)
//! carrying both portable identity and the local stat fingerprint;
//! `engine_state` is a typed singleton row (never a key/value registry); and
//! `intents` is the crash-recovery journal for in-flight apply operations.
//!
//! Portable `files` identity mutates only with a proven push/pull or intent
//! recovery. [`ManifestStore::refresh_local_file_records`] may separately
//! refresh local-only stat observations after byte verification without
//! changing the manifest identity.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;

use bowline_core::ids::ContentId;
use rusqlite::{Connection, OptionalExtension, params};

use super::manifest::{BlobKey, EntryKind, FileMode, KeyEpoch, ManifestKey, WorkspacePath};

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    symlink_target TEXT,
    content_id TEXT,
    blob_key TEXT,
    key_epoch INTEGER,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    dev INTEGER NOT NULL,
    hashed_at INTEGER,
    verified_at INTEGER
) STRICT;
CREATE TABLE IF NOT EXISTS engine_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    applied_manifest_key TEXT,
    last_ref_version INTEGER,
    highest_verified_ref_version INTEGER,
    highest_verified_manifest_key TEXT
) STRICT;
CREATE TABLE IF NOT EXISTS intents (
    path TEXT PRIMARY KEY,
    operation_kind TEXT NOT NULL,
    temp_name TEXT,
    expected_preimage TEXT,
    target_record TEXT,
    preserved_preimage TEXT,
    target_manifest_key TEXT,
    created_at INTEGER NOT NULL
) STRICT;";

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
    pub verified_at: Option<i64>,
}

/// The typed singleton engine-state row. Absent fields read as `None` before the
/// first commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineState {
    pub applied_manifest_key: Option<ManifestKey>,
    pub last_ref_version: Option<u64>,
    pub highest_verified_ref_version: Option<u64>,
    pub highest_verified_manifest_key: Option<ManifestKey>,
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
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(SCHEMA)?;
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

    /// The full ancestor as a sorted map — the three-way base for merge.
    pub fn all_files(&self) -> Result<BTreeMap<WorkspacePath, FileRecord>, ManifestStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT path, kind, size, mode, symlink_target, content_id, blob_key, key_epoch, \
             mtime_ns, ctime_ns, inode, dev, hashed_at, verified_at FROM files ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| Ok(row_to_file(row)))?;
        let mut files = BTreeMap::new();
        for row in rows {
            let (path, record) = row??;
            files.insert(path, record);
        }
        Ok(files)
    }

    /// The singleton engine-state row (default if not yet written).
    pub fn engine_state(&self) -> Result<EngineState, ManifestStoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT applied_manifest_key, last_ref_version, highest_verified_ref_version, \
                 highest_verified_manifest_key FROM engine_state WHERE singleton = 1",
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

    /// The ONLY place `files` changes on push. One transaction applies the
    /// ancestor mutation, advances `applied_manifest_key` + `last_ref_version`,
    /// and advances the freshness ratchet together.
    pub fn commit_push_success(
        &mut self,
        commit: &AncestorCommit,
        manifest_key: &ManifestKey,
        ref_version: u64,
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
            Ok(())
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
    ) -> Result<(), ManifestStoreError> {
        self.in_transaction(|connection| {
            apply_ancestor(connection, commit)?;
            if let Some((manifest_key, ref_version)) = applied {
                set_applied(connection, manifest_key, to_i64(ref_version)?)?;
            }
            if let Some((manifest_key, ref_version)) = verified {
                advance_verified_ratchet(connection, manifest_key, to_i64(ref_version)?)?;
            }
            let mut delete = connection.prepare("DELETE FROM intents WHERE path = ?1")?;
            for path in intent_ids {
                delete.execute(params![path.as_str()])?;
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
            record.verified_at,
        ])?;
    }
    Ok(())
}

fn set_applied(
    connection: &Connection,
    manifest_key: &ManifestKey,
    ref_version: i64,
) -> Result<(), ManifestStoreError> {
    connection.execute(
        "INSERT INTO engine_state (singleton, applied_manifest_key, last_ref_version) \
         VALUES (1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET \
         applied_manifest_key = excluded.applied_manifest_key, \
         last_ref_version = excluded.last_ref_version",
        params![manifest_key.as_str(), ref_version],
    )?;
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
        verified_at: row.get::<_, Option<i64>>(13)?,
    };
    Ok((path, record))
}

fn row_to_engine_state(row: &rusqlite::Row<'_>) -> Result<EngineState, ManifestStoreError> {
    Ok(EngineState {
        applied_manifest_key: row.get::<_, Option<String>>(0)?.map(ManifestKey::new),
        last_ref_version: row.get::<_, Option<i64>>(1)?.map(from_i64).transpose()?,
        highest_verified_ref_version: row.get::<_, Option<i64>>(2)?.map(from_i64).transpose()?,
        highest_verified_manifest_key: row.get::<_, Option<String>>(3)?.map(ManifestKey::new),
    })
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

#[derive(Debug)]
pub enum ManifestStoreError {
    Sqlite(rusqlite::Error),
    Io(io::Error),
    Corrupt { field: &'static str },
    ValueOutOfRange { field: &'static str },
}

impl fmt::Display for ManifestStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "manifest store SQLite failed: {error}"),
            Self::Io(error) => write!(formatter, "manifest store I/O failed: {error}"),
            Self::Corrupt { field } => write!(formatter, "manifest store corrupt value: {field}"),
            Self::ValueOutOfRange { field } => {
                write!(formatter, "manifest store value out of range: {field}")
            }
        }
    }
}

impl Error for ManifestStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Corrupt { .. } | Self::ValueOutOfRange { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for ManifestStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<io::Error> for ManifestStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
#[path = "store/tests.rs"]
mod tests;
