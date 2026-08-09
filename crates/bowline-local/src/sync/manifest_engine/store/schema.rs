//! Canonical SQLite schema for the manifest engine store.

pub(super) const SCHEMA: &str = "\
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
    materialization_revision INTEGER NOT NULL DEFAULT 0 CHECK (materialization_revision >= 0),
    highest_verified_ref_version INTEGER,
    highest_verified_manifest_key TEXT,
    CHECK ((applied_manifest_key IS NULL) = (last_ref_version IS NULL))
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
) STRICT;
CREATE TABLE IF NOT EXISTS unsyncable (
    path TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    errno INTEGER,
    observed_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS pending_push (
    path TEXT PRIMARY KEY
) STRICT;
CREATE TABLE IF NOT EXISTS blobs (
    content_id TEXT PRIMARY KEY,
    blob_key TEXT NOT NULL,
    key_epoch INTEGER NOT NULL,
    byte_len INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS tree_nodes (
    subtree_hash TEXT PRIMARY KEY,
    node_key TEXT NOT NULL,
    key_epoch INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS tree_node_epochs (
    node_key TEXT PRIMARY KEY,
    key_epoch INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS files_key_epoch_idx ON files(key_epoch);
CREATE INDEX IF NOT EXISTS tree_nodes_node_key_idx ON tree_nodes(node_key);";
