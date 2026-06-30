pub const CURRENT_SCHEMA_VERSION: u32 = 7;

pub const TABLES: &[&str] = &[
    "workspaces",
    "roots",
    "projects",
    "namespace_entries",
    "local_paths",
    "devices",
    "snapshots",
    "content_locators",
    "packs",
    "policies",
    "env_records",
    "setup_receipts",
    "events",
    "command_idempotency_records",
    "hydration_state",
    "conflicts",
    "leases",
    "work_views",
    "overlays",
    "indexes",
    "audit_log",
    "component_states",
    "workspace_sync_heads",
    "sync_operations",
    "sync_operation_checkpoints",
    "sync_remote_cursors",
    "projected_nodes",
    "hydration_queue",
    "local_write_log",
    "work_view_base_files",
    "index_documents",
    "symbol_records",
    "index_packs",
    "index_work",
    "hydration_budget_ledger",
    "storage_lifecycle_audit",
];

pub const SCHEMA_CORE: &str = r#"
	CREATE TABLE IF NOT EXISTS workspaces (
	  id TEXT PRIMARY KEY,
	  display_name TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS roots (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  accepted_path TEXT NOT NULL,
  state TEXT NOT NULL,
  materialization_state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(workspace_id, accepted_path)
);

CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  root_id TEXT NOT NULL REFERENCES roots(id) ON DELETE RESTRICT,
  path TEXT NOT NULL,
  hot_state TEXT NOT NULL,
  latest_snapshot_id TEXT,
  created_at TEXT NOT NULL,
  UNIQUE(workspace_id, path)
);

CREATE TABLE IF NOT EXISTS namespace_entries (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  classification TEXT NOT NULL,
  mode TEXT NOT NULL,
  content_id TEXT,
  hydration_state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, path)
);

CREATE TABLE IF NOT EXISTS local_paths (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  classification TEXT NOT NULL,
  mode TEXT NOT NULL,
  access_json TEXT NOT NULL DEFAULT '[]',
  matched_rule TEXT NOT NULL DEFAULT '',
  rule_source TEXT NOT NULL DEFAULT '',
  risk TEXT NOT NULL DEFAULT '',
  summary TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS devices (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  name TEXT NOT NULL,
  platform TEXT NOT NULL,
  trust_state TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS snapshots (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  kind TEXT NOT NULL,
  base_snapshot_id TEXT,
  manifest_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS packs (
  id TEXT NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  kind TEXT NOT NULL,
  byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
  object_hash TEXT NOT NULL DEFAULT '',
  key_epoch INTEGER NOT NULL DEFAULT 1 CHECK (key_epoch > 0),
  state TEXT NOT NULL,
  retain_until TEXT,
  created_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, id),
  CHECK (kind IN ('source-pack', 'large-chunk', 'snapshot-manifest', 'locator-index', 'index-pack', 'overlay-pack', 'agent-overlay')),
  CHECK (state IN ('pending', 'current', 'orphan-candidate', 'retained', 'delete-eligible'))
);

CREATE INDEX IF NOT EXISTS idx_packs_workspace_id
  ON packs (workspace_id, id);

CREATE TABLE IF NOT EXISTS content_locators (
  content_id TEXT NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  storage TEXT NOT NULL,
  raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
  pack_id TEXT,
  offset INTEGER CHECK (offset IS NULL OR offset >= 0),
  length INTEGER CHECK (length IS NULL OR length >= 0),
  locator_index_object_key TEXT,
  locator_index_hash TEXT,
  locator_index_key_epoch INTEGER NOT NULL DEFAULT 1 CHECK (locator_index_key_epoch > 0),
  locator_json TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, content_id),
  FOREIGN KEY (workspace_id, pack_id) REFERENCES packs(workspace_id, id) ON DELETE RESTRICT,
  CHECK (
    (storage = 'packed' AND pack_id IS NOT NULL AND offset IS NOT NULL AND length IS NOT NULL)
    OR
    (storage != 'packed' AND pack_id IS NULL AND offset IS NULL AND length IS NULL)
  )
);

CREATE INDEX IF NOT EXISTS idx_content_locators_workspace_content
  ON content_locators (workspace_id, content_id);

CREATE TABLE IF NOT EXISTS storage_lifecycle_audit (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  object_key TEXT NOT NULL,
  previous_state TEXT NOT NULL,
  next_state TEXT NOT NULL,
  reason TEXT NOT NULL,
  actor_device_id TEXT,
  occurred_at TEXT NOT NULL,
  CHECK (previous_state IN ('pending', 'current', 'orphan-candidate', 'retained', 'delete-eligible')),
  CHECK (next_state IN ('pending', 'current', 'orphan-candidate', 'retained', 'delete-eligible'))
);

CREATE INDEX IF NOT EXISTS idx_storage_lifecycle_audit_workspace_time
  ON storage_lifecycle_audit (workspace_id, occurred_at, id);

CREATE TABLE IF NOT EXISTS policies (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  version TEXT NOT NULL,
  policy_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS env_records (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  source_path TEXT NOT NULL,
	  key_name TEXT NOT NULL,
	  access TEXT NOT NULL,
	  value_ciphertext_ref TEXT,
	  profile TEXT NOT NULL DEFAULT 'default',
	  occurrence_index INTEGER NOT NULL DEFAULT 0,
	  line_kind TEXT NOT NULL DEFAULT 'key-value',
	  encrypted_locator_json TEXT NOT NULL DEFAULT '{}',
	  format_json TEXT NOT NULL DEFAULT '{}',
	  materialization_state TEXT NOT NULL DEFAULT 'pending',
	  restriction_state TEXT NOT NULL DEFAULT 'unrestricted',
	  key_epoch INTEGER NOT NULL DEFAULT 1,
	  metadata_json TEXT NOT NULL DEFAULT '{}',
	  updated_at TEXT NOT NULL
	);

CREATE TABLE IF NOT EXISTS setup_receipts (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
	  command TEXT NOT NULL,
	  state TEXT NOT NULL,
	  receipt_json TEXT NOT NULL,
	  recipe_hash TEXT NOT NULL DEFAULT '',
	  approval_state TEXT NOT NULL DEFAULT 'not-required',
	  trigger TEXT NOT NULL DEFAULT 'unknown',
	  cwd TEXT NOT NULL DEFAULT '',
	  os TEXT NOT NULL DEFAULT '',
	  arch TEXT NOT NULL DEFAULT '',
	  env_profile TEXT NOT NULL DEFAULT 'default',
	  output_path TEXT,
	  redacted_summary TEXT NOT NULL DEFAULT '',
	  updated_at TEXT NOT NULL
	);

CREATE TABLE IF NOT EXISTS events (
  id TEXT PRIMARY KEY,
  schema_version INTEGER NOT NULL,
  name TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  severity TEXT NOT NULL,
  summary TEXT NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT,
  path TEXT,
  lease_id TEXT,
  device_id TEXT,
  subject_json TEXT,
  actor_json TEXT,
  payload_json TEXT NOT NULL,
  causation_id TEXT,
  correlation_id TEXT,
  redaction_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_occurred_id
  ON events (occurred_at, id);

CREATE TABLE IF NOT EXISTS command_idempotency_records (
  workspace_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  command TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  result_json TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_command_idempotency_records_expiry
  ON command_idempotency_records (expires_at);

CREATE TABLE IF NOT EXISTS hydration_state (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conflicts (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  state TEXT NOT NULL,
  conflict_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS leases (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  state TEXT NOT NULL,
  lease_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS overlays (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  lease_id TEXT,
  state TEXT NOT NULL,
  overlay_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS indexes (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  kind TEXT NOT NULL,
  state TEXT NOT NULL,
  watermark TEXT,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  subject TEXT NOT NULL,
  audit_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS component_states (
  component TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
	);
	"#;

pub const SCHEMA_INDEXING: &str = r#"
CREATE TABLE IF NOT EXISTS index_documents (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  snapshot_id TEXT,
  content_id TEXT,
  classification TEXT NOT NULL,
  mode TEXT NOT NULL,
  access_json TEXT NOT NULL DEFAULT '[]',
  policy_summary TEXT NOT NULL DEFAULT '',
  body_text TEXT NOT NULL DEFAULT '',
  hydration_state TEXT NOT NULL,
  indexed_bytes INTEGER NOT NULL DEFAULT 0 CHECK (indexed_bytes >= 0),
  source_watermark INTEGER NOT NULL DEFAULT 0 CHECK (source_watermark >= 0),
  indexed_watermark INTEGER NOT NULL DEFAULT 0 CHECK (indexed_watermark >= 0),
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, project_id, path)
);

CREATE INDEX IF NOT EXISTS idx_index_documents_project_state
  ON index_documents (workspace_id, project_id, state, updated_at);

CREATE TABLE IF NOT EXISTS symbol_records (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  path TEXT NOT NULL,
  snapshot_id TEXT,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  language TEXT NOT NULL,
  line_start INTEGER NOT NULL CHECK (line_start >= 0),
  line_end INTEGER NOT NULL CHECK (line_end >= 0),
  byte_start INTEGER NOT NULL CHECK (byte_start >= 0),
  byte_end INTEGER NOT NULL CHECK (byte_end >= 0),
  parser_status TEXT NOT NULL,
  access_json TEXT NOT NULL DEFAULT '[]',
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_symbol_records_lookup
  ON symbol_records (workspace_id, project_id, name, path);

CREATE TABLE IF NOT EXISTS index_packs (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  snapshot_id TEXT,
  object_key TEXT NOT NULL,
  byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
  hash TEXT NOT NULL,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, object_key)
);

CREATE TABLE IF NOT EXISTS index_work (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT,
  kind TEXT NOT NULL,
  source_watermark INTEGER NOT NULL DEFAULT 0 CHECK (source_watermark >= 0),
  indexed_watermark INTEGER NOT NULL DEFAULT 0 CHECK (indexed_watermark >= 0),
  state TEXT NOT NULL,
  reason TEXT,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_index_work_ready
  ON index_work (workspace_id, project_id, state, updated_at);

CREATE TABLE IF NOT EXISTS hydration_budget_ledger (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  lease_id TEXT,
  path TEXT NOT NULL,
  content_id TEXT,
  cause TEXT NOT NULL,
  requested_bytes INTEGER NOT NULL CHECK (requested_bytes >= 0),
  reserved_bytes INTEGER NOT NULL CHECK (reserved_bytes >= 0),
  committed_bytes INTEGER NOT NULL CHECK (committed_bytes >= 0),
  outcome TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

	CREATE INDEX IF NOT EXISTS idx_hydration_budget_lease
	  ON hydration_budget_ledger (workspace_id, lease_id, outcome, updated_at);
	"#;

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
  state TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  base_version INTEGER CHECK (base_version IS NULL OR base_version >= 0),
  base_snapshot_id TEXT,
  target_snapshot_id TEXT,
  device_id TEXT,
  payload_json TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  claimed_by TEXT,
  heartbeat_at TEXT,
  next_attempt_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_sync_operations_ready
  ON sync_operations (workspace_id, state, next_attempt_at, updated_at);

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

CREATE TABLE IF NOT EXISTS hydration_queue (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  content_id TEXT,
  priority TEXT NOT NULL,
  state TEXT NOT NULL,
  cause TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, path, cause)
);

CREATE INDEX IF NOT EXISTS idx_hydration_queue_ready
  ON hydration_queue (workspace_id, state, priority, updated_at);

CREATE TABLE IF NOT EXISTS projected_nodes (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  node_id TEXT NOT NULL,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  parent_node_id TEXT,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  content_id TEXT,
  hydration_state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, node_id),
  UNIQUE(workspace_id, path)
);

CREATE INDEX IF NOT EXISTS idx_projected_nodes_project_path
  ON projected_nodes (workspace_id, project_id, path);

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
	"#;

pub const SCHEMA_ENV_SETUP_INDEXES: &str = r#"
	CREATE INDEX IF NOT EXISTS idx_env_records_workspace_source_key
  ON env_records (workspace_id, source_path, key_name, occurrence_index);

CREATE INDEX IF NOT EXISTS idx_env_records_project_materialization
  ON env_records (workspace_id, project_id, materialization_state);

CREATE INDEX IF NOT EXISTS idx_setup_receipts_workspace_project_state
  ON setup_receipts (workspace_id, project_id, state, updated_at);

	CREATE INDEX IF NOT EXISTS idx_setup_receipts_recipe_hash
	  ON setup_receipts (workspace_id, project_id, recipe_hash);
	"#;

pub const SCHEMA_WORK_VIEWS: &str = r#"
	CREATE TABLE IF NOT EXISTS work_views (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  project_path TEXT NOT NULL,
  name TEXT NOT NULL,
  visible_path TEXT NOT NULL,
  base_snapshot_id TEXT NOT NULL,
  overlay_head TEXT NOT NULL,
  overlay_version INTEGER NOT NULL DEFAULT 0,
  env_profile TEXT NOT NULL DEFAULT 'default',
  lifecycle TEXT NOT NULL,
  visibility TEXT NOT NULL,
  sync_state TEXT NOT NULL,
  retention_state TEXT NOT NULL,
  retain_until TEXT,
  restorable INTEGER NOT NULL DEFAULT 1,
  owner_device_id TEXT,
  followed_by_json TEXT NOT NULL DEFAULT '[]',
  host_materializations_json TEXT NOT NULL DEFAULT '[]',
  attention_json TEXT NOT NULL DEFAULT '[]',
  audit_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, project_id, name),
  UNIQUE(workspace_id, visible_path),
  CHECK (lifecycle IN ('active', 'review-ready', 'accepted', 'discarded', 'expired', 'archived')),
  CHECK (visibility IN ('default-visible', 'hidden', 'pinned', 'followed')),
  CHECK (sync_state IN ('local-only', 'synced', 'uploading', 'attention', 'conflicted')),
  CHECK (retention_state IN ('current', 'retained', 'expired', 'delete-eligible')),
  CHECK (restorable IN (0, 1))
);

CREATE INDEX IF NOT EXISTS idx_work_views_workspace_visible
  ON work_views (workspace_id, visibility, lifecycle, updated_at);

CREATE INDEX IF NOT EXISTS idx_work_views_project_name
  ON work_views (workspace_id, project_id, name);

CREATE TABLE IF NOT EXISTS work_view_base_files (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  work_view_id TEXT NOT NULL REFERENCES work_views(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  hash TEXT NOT NULL,
  captured_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, work_view_id, path)
);

	CREATE INDEX IF NOT EXISTS idx_work_view_base_files_work_view
	  ON work_view_base_files (workspace_id, work_view_id);
	"#;
