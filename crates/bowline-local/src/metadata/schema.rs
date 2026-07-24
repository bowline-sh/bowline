pub const CURRENT_SCHEMA_VERSION: u32 = 34;

/// The complete live metadata schema after the Plan 111 cutover.
pub const TABLES: &[&str] = &[
    "workspaces",
    "roots",
    "projects",
    "local_paths",
    "env_records",
    "setup_receipts",
    "events",
    "work_views",
    "indexes",
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
  lifecycle_state TEXT NOT NULL DEFAULT 'active',
  local_materialization_state TEXT NOT NULL DEFAULT 'materialized',
  purge_after TEXT,
  latest_snapshot_id TEXT,
  git_observer_state TEXT NOT NULL DEFAULT 'ok',
  created_at TEXT NOT NULL,
  UNIQUE(workspace_id, path),
  CHECK (lifecycle_state IN ('active', 'archived', 'purge-pending', 'purged')),
  CHECK (local_materialization_state IN ('materialized', 'forgotten'))
);

CREATE TABLE IF NOT EXISTS local_paths (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  classification TEXT NOT NULL,
  mode TEXT NOT NULL,
  access_json TEXT NOT NULL DEFAULT '[]',
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
	  setup_identity_hash TEXT NOT NULL DEFAULT '',
	  readiness_state TEXT NOT NULL DEFAULT 'unknown',
	  readiness_reason TEXT NOT NULL DEFAULT '',
	  readiness_remedy TEXT NOT NULL DEFAULT '',
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

CREATE TABLE IF NOT EXISTS indexes (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  kind TEXT NOT NULL,
  state TEXT NOT NULL,
  watermark TEXT,
  updated_at TEXT NOT NULL
);

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

CREATE INDEX IF NOT EXISTS idx_setup_receipts_identity_readiness
  ON setup_receipts (workspace_id, project_id, setup_identity_hash, readiness_state, updated_at);
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
  base_descriptor_version INTEGER,
  exposed_snapshot_id TEXT,
  policy_fingerprint TEXT,
  materialized_overlay_root_id TEXT,
  materialized_overlay_manifest_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, project_id, name),
  UNIQUE(workspace_id, visible_path),
  CHECK (lifecycle IN ('active', 'review-ready', 'accepted', 'discarded')),
  CHECK (visibility IN ('default-visible', 'hidden', 'pinned', 'followed')),
  CHECK (sync_state IN ('local-only', 'synced', 'uploading', 'attention', 'conflicted')),
  CHECK (retention_state IN ('current', 'retained', 'expired', 'delete-eligible')),
  CHECK (restorable IN (0, 1))
);

CREATE INDEX IF NOT EXISTS idx_work_views_workspace_visible
  ON work_views (workspace_id, visibility, lifecycle, updated_at);

CREATE INDEX IF NOT EXISTS idx_work_views_project_name
  ON work_views (workspace_id, project_id, name);
	"#;

pub const CURRENT_SCHEMA_BATCHES: &[&str] =
    &[SCHEMA_CORE, SCHEMA_ENV_SETUP_INDEXES, SCHEMA_WORK_VIEWS];
