pub const SCHEMA_MATERIALIZATION: &str = r#"
CREATE TABLE IF NOT EXISTS workspace_sync_heads (
  workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE RESTRICT,
  version INTEGER NOT NULL CHECK (version >= 0),
  snapshot_id TEXT NOT NULL,
  updated_at_tick INTEGER NOT NULL CHECK (updated_at_tick >= 0),
  updated_by_device_id TEXT,
  observed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_operations (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  kind TEXT NOT NULL,
  resource_key TEXT NOT NULL,
  state TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  base_version INTEGER CHECK (base_version IS NULL OR base_version >= 0),
  base_snapshot_id TEXT,
  target_snapshot_id TEXT,
  device_id TEXT,
  payload_json TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  claimed_by TEXT,
  claim_token TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  heartbeat_at TEXT,
  lease_expires_at TEXT,
  cancellation_requested_at TEXT,
  next_attempt_at TEXT,
  result_json TEXT,
  last_error_code TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, idempotency_key),
  CHECK (kind IN ('daemon-reconcile', 'conflict-occurrence-reconcile', 'work-view-overlay-sync')),
  CHECK (
    (kind = 'daemon-reconcile' AND resource_key = ('workspace_sync:' || workspace_id))
    OR
    (kind = 'conflict-occurrence-reconcile' AND resource_key LIKE ('conflict_followup:' || workspace_id || ':%'))
    OR
    (kind = 'work-view-overlay-sync' AND resource_key = ('post_commit:' || workspace_id))
  ),
  CHECK (state IN ('queued', 'claimed', 'waiting_retry', 'blocked_offline', 'reconciliation_required', 'attention', 'completed', 'cancelled')),
  CHECK (
    (state = 'claimed' AND claimed_by IS NOT NULL AND claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    OR
    (state != 'claimed' AND claimed_by IS NULL AND claim_token IS NULL AND lease_expires_at IS NULL)
  )
);

CREATE INDEX IF NOT EXISTS idx_sync_operations_ready
  ON sync_operations (workspace_id, state, next_attempt_at, updated_at);

CREATE INDEX IF NOT EXISTS idx_sync_operations_workspace_state_updated
  ON sync_operations (workspace_id, state, updated_at, id);

CREATE INDEX IF NOT EXISTS idx_sync_operations_claim_lease
  ON sync_operations (workspace_id, state, lease_expires_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_operations_one_claim_per_resource
  ON sync_operations (resource_key)
  WHERE state = 'claimed';

CREATE TABLE IF NOT EXISTS sync_operation_checkpoints (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  operation_id TEXT NOT NULL REFERENCES sync_operations(id) ON DELETE CASCADE,
  step TEXT NOT NULL,
  state TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_operation_checkpoints_operation
  ON sync_operation_checkpoints (operation_id, created_at, id);

CREATE TABLE IF NOT EXISTS sync_remote_cursors (
  workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE RESTRICT,
  cursor TEXT,
  last_observed_version INTEGER CHECK (last_observed_version IS NULL OR last_observed_version >= 0),
  last_observed_snapshot_id TEXT,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS materialization_tasks (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  snapshot_id TEXT NOT NULL,
  path TEXT NOT NULL,
  expected_kind TEXT NOT NULL,
  expected_content_id TEXT,
  expected_byte_len INTEGER NOT NULL CHECK (expected_byte_len >= 0),
  expected_executable INTEGER NOT NULL DEFAULT 0 CHECK (expected_executable IN (0, 1)),
  priority_class TEXT NOT NULL,
  state TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  not_before TEXT,
  claim_token TEXT,
  claimed_by TEXT,
  claimed_at TEXT,
  lease_expires_at TEXT,
  last_error_kind TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK (expected_kind IN ('directory', 'file', 'symlink', 'tombstone')),
  CHECK (priority_class IN (
    'correctness-critical',
    'active-project',
    'requested-path',
    'recent-project',
    'small-file',
    'background-large',
    'cleanup'
  )),
  CHECK (state IN (
    'queued',
    'claimed',
    'staged',
    'waiting-retry',
    'blocked-offline',
    'blocked-missing',
    'blocked-conflict',
    'attention',
    'ready',
    'cancelled'
  )),
  CHECK ((state = 'claimed') = (claim_token IS NOT NULL)),
  CHECK ((state = 'claimed') = (claimed_by IS NOT NULL)),
  CHECK ((state = 'claimed') = (claimed_at IS NOT NULL)),
  CHECK ((state = 'claimed') = (lease_expires_at IS NOT NULL)),
  CHECK (last_error_kind IS NULL OR last_error_kind IN (
    'path-fence-not-current',
    'content-missing',
    'transport-unavailable',
    'remote-timeout',
    'remote-service-unavailable',
    'remote-rate-limited',
    'authorization-required',
    'content-integrity-failed',
    'hydration-failed',
    'local-io-failed',
    'invalid-hydration-metadata',
    'unsupported-hydration',
    'workspace-mutation-failed',
    'retry-budget-exhausted'
  ))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_materialization_tasks_identity
  ON materialization_tasks (
    workspace_id,
    snapshot_id,
    path,
    COALESCE(expected_content_id, '')
  );

CREATE INDEX IF NOT EXISTS idx_materialization_tasks_ready
  ON materialization_tasks (
    workspace_id,
    state,
    priority_class,
    not_before,
    lease_expires_at,
    created_at,
    id
  );

CREATE INDEX IF NOT EXISTS idx_materialization_tasks_snapshot_path
  ON materialization_tasks (workspace_id, snapshot_id, path);

CREATE TABLE IF NOT EXISTS materialization_path_states (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  snapshot_id TEXT,
  expected_content_id TEXT,
  state TEXT NOT NULL,
  observed_content_id TEXT,
  observed_byte_len INTEGER CHECK (observed_byte_len IS NULL OR observed_byte_len >= 0),
  source_hydration_state TEXT,
  verified_at TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, path),
  CHECK (state IN (
    'needs-observation',
    'queued',
    'materializing',
    'blocked-offline',
    'blocked-missing',
    'blocked-conflict',
    'attention',
    'ready',
    'excluded'
  ))
);

CREATE INDEX IF NOT EXISTS idx_materialization_path_states_snapshot_state
  ON materialization_path_states (workspace_id, snapshot_id, state, path);

CREATE TABLE IF NOT EXISTS local_write_log (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  device_id TEXT NOT NULL,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  source_path TEXT,
  operation TEXT NOT NULL,
  staged_content_id TEXT,
  policy_classification TEXT NOT NULL,
  causation_id TEXT NOT NULL,
  settled_at TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_local_write_log_workspace_created
  ON local_write_log (workspace_id, created_at, id);

CREATE INDEX IF NOT EXISTS idx_local_write_log_project_created
  ON local_write_log (workspace_id, project_id, created_at, id);

CREATE INDEX IF NOT EXISTS idx_local_write_log_path_created
  ON local_write_log (workspace_id, path, created_at, id);

CREATE TABLE IF NOT EXISTS scan_stat_cache (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  ctime_ns INTEGER NOT NULL,
  inode INTEGER NOT NULL,
  dev INTEGER NOT NULL,
  file_mode INTEGER NOT NULL,
  key_epoch INTEGER NOT NULL CHECK (key_epoch > 0),
  content_key_fingerprint TEXT NOT NULL,
  content_id TEXT NOT NULL,
  byte_len INTEGER NOT NULL,
  format_version INTEGER NOT NULL CHECK (format_version > 0),
  hashed_at_ns INTEGER NOT NULL,
  last_verified_at TEXT NOT NULL,
  -- Change-frontier projection (Plan 06 U7a). Pure functions of `path`, kept by
  -- SQLite so the root-level index can seek only root rows instead of scanning
  -- the whole workspace. `path_depth` = count of '/' separators (root-level = 0);
  -- `root_segment` = the first path segment (top-level project/dir name).
  path_depth INTEGER GENERATED ALWAYS AS (length(path) - length(replace(path, '/', ''))) VIRTUAL,
  root_segment TEXT GENERATED ALWAYS AS (
    CASE WHEN instr(path, '/') = 0 THEN path ELSE substr(path, 1, instr(path, '/') - 1) END
  ) VIRTUAL,
  PRIMARY KEY (workspace_id, path)
);

CREATE INDEX IF NOT EXISTS idx_scan_stat_cache_root_level
  ON scan_stat_cache (workspace_id, path_depth, path);

CREATE INDEX IF NOT EXISTS idx_scan_stat_cache_root_segment
  ON scan_stat_cache (workspace_id, root_segment, path);

"#;
