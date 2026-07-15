pub const CURRENT_SCHEMA_VERSION: u32 = 29;

pub const TABLES: &[&str] = &[
    "workspaces",
    "roots",
    "projects",
    "local_paths",
    "devices",
    "snapshots",
    "snapshot_roots",
    "metadata_identity_contexts",
    "metadata_records",
    "metadata_object_bindings",
    "metadata_record_edges",
    "namespace_pages",
    "content_layouts",
    "segment_pages",
    "current_namespace_entries",
    "snapshot_pins",
    "metadata_gc_queue",
    "metadata_gc_checkpoints",
    "content_locators",
    "packs",
    "policies",
    "env_records",
    "setup_receipts",
    "events",
    "hydration_state",
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
    "materialization_tasks",
    "materialization_path_states",
    "preparation_leases",
    "prepared_staged_content",
    "local_write_log",
    "work_view_base_descriptors",
    "work_view_accept_operations",
    "work_view_accept_checkpoints",
    "agent_mcp_tokens",
    "storage_lifecycle_audit",
    "merge_plugin_approvals",
    "scan_stat_cache",
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
  root_id TEXT NOT NULL,
  semantic_manifest_digest TEXT NOT NULL,
  entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
  refs_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(workspace_id, id)
);

CREATE TABLE IF NOT EXISTS metadata_identity_contexts (
  workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
  key_hex TEXT NOT NULL CHECK (length(key_hex) = 64),
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS metadata_records (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  logical_id TEXT NOT NULL,
  record_kind TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, record_kind, logical_id),
  UNIQUE(workspace_id, logical_id),
  CHECK (record_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page'))
);

CREATE TABLE IF NOT EXISTS metadata_object_bindings (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  logical_id TEXT NOT NULL,
  record_kind TEXT NOT NULL,
  object_key TEXT NOT NULL,
  byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
  object_hash TEXT NOT NULL,
  key_epoch INTEGER NOT NULL CHECK (key_epoch > 0),
  verification_state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  verified_at TEXT,
  PRIMARY KEY (workspace_id, record_kind, logical_id),
  UNIQUE(workspace_id, object_key),
  FOREIGN KEY (workspace_id, record_kind, logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  CHECK (record_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page')),
  CHECK (verification_state IN ('unverified', 'verified', 'rejected'))
);

CREATE TABLE IF NOT EXISTS metadata_record_edges (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  parent_kind TEXT NOT NULL,
  parent_logical_id TEXT NOT NULL,
  child_kind TEXT NOT NULL,
  child_logical_id TEXT NOT NULL,
  PRIMARY KEY (workspace_id, parent_kind, parent_logical_id, child_kind, child_logical_id),
  FOREIGN KEY (workspace_id, parent_kind, parent_logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id, child_kind, child_logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  CHECK (parent_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page')),
  CHECK (child_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page'))
);

CREATE INDEX IF NOT EXISTS idx_metadata_record_edges_child
  ON metadata_record_edges (workspace_id, child_kind, child_logical_id);

CREATE TABLE IF NOT EXISTS namespace_pages (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  logical_id TEXT NOT NULL,
  record_kind TEXT NOT NULL DEFAULT 'namespace-page' CHECK (record_kind = 'namespace-page'),
  cache_path TEXT,
  encoded_bytes INTEGER NOT NULL CHECK (encoded_bytes >= 0),
  cache_state TEXT NOT NULL,
  last_accessed_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, logical_id),
  FOREIGN KEY (workspace_id, record_kind, logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  CHECK (cache_state IN ('absent', 'present', 'deleting', 'corrupt')),
  CHECK ((cache_state IN ('present', 'deleting')) = (cache_path IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS content_layouts (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  logical_id TEXT NOT NULL,
  record_kind TEXT NOT NULL DEFAULT 'content-layout' CHECK (record_kind = 'content-layout'),
  cache_path TEXT,
  encoded_bytes INTEGER NOT NULL CHECK (encoded_bytes >= 0),
  cache_state TEXT NOT NULL,
  last_accessed_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, logical_id),
  FOREIGN KEY (workspace_id, record_kind, logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  CHECK (cache_state IN ('absent', 'present', 'deleting', 'corrupt')),
  CHECK ((cache_state IN ('present', 'deleting')) = (cache_path IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS segment_pages (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  logical_id TEXT NOT NULL,
  record_kind TEXT NOT NULL DEFAULT 'segment-page' CHECK (record_kind = 'segment-page'),
  cache_path TEXT,
  encoded_bytes INTEGER NOT NULL CHECK (encoded_bytes >= 0),
  cache_state TEXT NOT NULL,
  last_accessed_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, logical_id),
  FOREIGN KEY (workspace_id, record_kind, logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE CASCADE,
  CHECK (cache_state IN ('absent', 'present', 'deleting', 'corrupt')),
  CHECK ((cache_state IN ('present', 'deleting')) = (cache_path IS NOT NULL))
);

CREATE TABLE IF NOT EXISTS snapshot_roots (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  snapshot_id TEXT NOT NULL,
  root_kind TEXT NOT NULL,
  record_kind TEXT NOT NULL,
  logical_id TEXT NOT NULL,
  committed_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, snapshot_id, root_kind, record_kind, logical_id),
  FOREIGN KEY (workspace_id, snapshot_id) REFERENCES snapshots(workspace_id, id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id, record_kind, logical_id)
    REFERENCES metadata_records(workspace_id, record_kind, logical_id) ON DELETE RESTRICT,
  CHECK (root_kind IN ('namespace', 'extra')),
  CHECK (record_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_snapshot_roots_one_namespace
  ON snapshot_roots (workspace_id, snapshot_id)
  WHERE root_kind = 'namespace';

CREATE TABLE IF NOT EXISTS current_namespace_entries (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  snapshot_id TEXT NOT NULL,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  component_prefix TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  classification TEXT NOT NULL,
  mode TEXT NOT NULL,
  access_json TEXT NOT NULL,
  content_id TEXT,
  content_layout_id TEXT,
  symlink_target TEXT,
  byte_len INTEGER CHECK (byte_len IS NULL OR byte_len >= 0),
  executability TEXT NOT NULL,
  hydration_state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, path),
  FOREIGN KEY (workspace_id, snapshot_id) REFERENCES snapshots(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_current_namespace_component_path
  ON current_namespace_entries (workspace_id, component_prefix, path);

CREATE INDEX IF NOT EXISTS idx_current_namespace_snapshot
  ON current_namespace_entries (workspace_id, snapshot_id, path);

CREATE TABLE IF NOT EXISTS snapshot_pins (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  snapshot_id TEXT NOT NULL,
  root_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  expires_at TEXT,
  created_at TEXT NOT NULL,
  FOREIGN KEY (workspace_id, snapshot_id) REFERENCES snapshots(workspace_id, id) ON DELETE CASCADE,
  UNIQUE(workspace_id, snapshot_id, reason, owner_kind, owner_id),
  CHECK (reason IN ('workspace-ref', 'project-ref', 'work-view', 'conflict', 'durable-operation', 'explicit-history')),
  CHECK (owner_kind IN ('workspace-ref', 'project-ref', 'work-view', 'conflict', 'durable-operation', 'explicit-history'))
);

CREATE INDEX IF NOT EXISTS idx_snapshot_pins_live
  ON snapshot_pins (workspace_id, expires_at, snapshot_id);

CREATE TABLE IF NOT EXISTS metadata_gc_checkpoints (
  workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
  generation TEXT NOT NULL,
  phase TEXT NOT NULL,
  sweep_cursor_kind TEXT,
  sweep_cursor_id TEXT,
  grace_before TEXT NOT NULL,
  started_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK (phase IN ('mark', 'sweep', 'complete'))
);

CREATE TABLE IF NOT EXISTS metadata_gc_queue (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  generation TEXT NOT NULL,
  record_kind TEXT NOT NULL,
  logical_id TEXT NOT NULL,
  state TEXT NOT NULL,
  enqueued_at TEXT NOT NULL,
  processed_at TEXT,
  PRIMARY KEY (workspace_id, generation, record_kind, logical_id),
  CHECK (record_kind IN ('snapshot-root', 'namespace-page', 'content-layout', 'segment-page')),
  CHECK (state IN ('pending', 'marked', 'delete-eligible'))
);

CREATE INDEX IF NOT EXISTS idx_metadata_gc_queue_work
  ON metadata_gc_queue (workspace_id, generation, state, record_kind, logical_id);

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
  CHECK (kind IN ('source-pack', 'overlay-pack', 'agent-overlay')),
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

CREATE TABLE IF NOT EXISTS agent_mcp_tokens (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  lease_id TEXT NOT NULL REFERENCES leases(id) ON DELETE RESTRICT,
  token_hash TEXT NOT NULL UNIQUE,
  token_file TEXT NOT NULL,
  grants_json TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT,
  created_at TEXT NOT NULL,
  last_used_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_agent_mcp_tokens_lease
  ON agent_mcp_tokens (lease_id, revoked_at, expires_at);

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

CREATE TABLE IF NOT EXISTS hydration_state (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  path TEXT NOT NULL,
  state TEXT NOT NULL,
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

pub const SCHEMA_MERGE_PLUGIN_APPROVALS: &str = r#"
CREATE TABLE IF NOT EXISTS merge_plugin_approvals (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  plugin_id TEXT NOT NULL,
  plugin_version TEXT NOT NULL,
  digest TEXT NOT NULL,
  matcher_version TEXT NOT NULL,
  validator_version TEXT NOT NULL,
  state TEXT NOT NULL,
  approved_by_device_id TEXT NOT NULL,
  approved_at TEXT NOT NULL,
  PRIMARY KEY (
    workspace_id,
    plugin_id,
    plugin_version,
    digest,
    matcher_version,
    validator_version
  ),
  CHECK (state IN ('approved', 'revoked'))
);

CREATE INDEX IF NOT EXISTS idx_merge_plugin_approvals_workspace
  ON merge_plugin_approvals (workspace_id, state, plugin_id);
"#;

#[path = "schema/materialization.rs"]
mod materialization;

pub use materialization::SCHEMA_MATERIALIZATION;

pub const SCHEMA_WORK_VIEW_ACCEPT_OPERATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS work_view_accept_operations (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  work_view_id TEXT NOT NULL REFERENCES work_views(id) ON DELETE RESTRICT,
  device_id TEXT NOT NULL,
  resource_key TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  state TEXT NOT NULL,
  selected_paths_json TEXT NOT NULL,
  input_json TEXT NOT NULL,
  observed_main_snapshot_id TEXT,
  observed_ref_version INTEGER CHECK (observed_ref_version IS NULL OR observed_ref_version >= 0),
  observed_ref_snapshot_id TEXT,
  target_snapshot_id TEXT,
  result_json TEXT,
  review_reason TEXT CHECK (
    review_reason IS NULL OR review_reason IN ('policy-drift', 'merge-conflict')
  ),
  failure_reason TEXT CHECK (
    failure_reason IS NULL OR failure_reason IN ('transient', 'permanent')
  ),
  cancellation_requested_at TEXT,
  last_error TEXT,
  claimed_by TEXT,
  claim_token TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  heartbeat_at TEXT,
  lease_expires_at TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_attempt_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(workspace_id, idempotency_key),
  CHECK (resource_key = (
    'work_view_accept:' || workspace_id || ':' || project_id || ':' || work_view_id
  )),
  CHECK (state IN (
    'queued', 'claimed', 'waiting-retry', 'review-required', 'completed', 'cancelled', 'failed'
  )),
  CHECK (
    (state = 'claimed' AND claimed_by IS NOT NULL AND claim_token IS NOT NULL
      AND heartbeat_at IS NOT NULL AND lease_expires_at IS NOT NULL)
    OR
    (state != 'claimed' AND claimed_by IS NULL AND claim_token IS NULL
      AND heartbeat_at IS NULL AND lease_expires_at IS NULL)
  ),
  CHECK ((state = 'review-required') = (review_reason IS NOT NULL)),
  CHECK ((state IN ('waiting-retry', 'failed')) = (failure_reason IS NOT NULL)),
  CHECK (state != 'completed' OR result_json IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_work_view_accept_ready
  ON work_view_accept_operations (state, next_attempt_at, created_at, id);

CREATE INDEX IF NOT EXISTS idx_work_view_accept_claim_lease
  ON work_view_accept_operations (state, lease_expires_at, id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_work_view_accept_one_active
  ON work_view_accept_operations (workspace_id, work_view_id)
  WHERE state IN ('queued', 'claimed', 'waiting-retry');

CREATE TABLE IF NOT EXISTS work_view_accept_checkpoints (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  operation_id TEXT NOT NULL REFERENCES work_view_accept_operations(id) ON DELETE CASCADE,
  claim_generation INTEGER NOT NULL CHECK (claim_generation > 0),
  step TEXT NOT NULL CHECK (step IN (
    'candidate-built', 'main-fence-rechecked', 'objects-uploaded', 'snapshot-staged',
    'main-published', 'workspace-ref-published', 'lifecycle-published'
  )),
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_work_view_accept_checkpoints_operation
  ON work_view_accept_checkpoints (operation_id, created_at, id);
"#;

pub const SCHEMA_PREPARATION: &str = r#"
CREATE TABLE IF NOT EXISTS preparation_leases (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  snapshot_candidate_id TEXT NOT NULL,
  owner_marker TEXT NOT NULL,
  state TEXT NOT NULL,
  reservation_bytes INTEGER NOT NULL CHECK (reservation_bytes >= 0),
  prepared_at TEXT,
  referenced_at TEXT,
  finished_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (id, owner_marker),
  CHECK (state IN ('preparing', 'prepared', 'referenced-by-upload', 'committed', 'abandoned')),
  CHECK ((state IN ('committed', 'abandoned')) = (finished_at IS NOT NULL)),
  CHECK (state NOT IN ('prepared', 'referenced-by-upload', 'committed') OR prepared_at IS NOT NULL),
  CHECK (state NOT IN ('referenced-by-upload', 'committed') OR referenced_at IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_preparation_leases_workspace_state
  ON preparation_leases (workspace_id, state, updated_at, id);

CREATE TABLE IF NOT EXISTS prepared_staged_content (
  lease_id TEXT NOT NULL,
  content_id TEXT NOT NULL,
  staged_path TEXT NOT NULL UNIQUE,
  logical_size INTEGER NOT NULL CHECK (logical_size >= 0),
  source_fingerprint TEXT NOT NULL,
  owner_marker TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (lease_id, content_id),
  FOREIGN KEY (lease_id, owner_marker)
    REFERENCES preparation_leases(id, owner_marker) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_prepared_staged_content_owner_age
  ON prepared_staged_content (owner_marker, updated_at, staged_path);
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
  base_review_reason TEXT CHECK (
    base_review_reason IS NULL OR base_review_reason = 'legacy-base-unverifiable'
  ),
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

CREATE TABLE IF NOT EXISTS work_view_base_descriptors (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
  work_view_id TEXT NOT NULL REFERENCES work_views(id) ON DELETE CASCADE,
  format_version INTEGER NOT NULL CHECK (format_version > 0),
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  base_snapshot_id TEXT NOT NULL,
  project_prefix TEXT NOT NULL,
  policy_fingerprint TEXT NOT NULL,
  exposed_snapshot_id TEXT NOT NULL,
  exposed_namespace_root_id TEXT NOT NULL,
  exposed_semantic_manifest_digest TEXT NOT NULL,
  exposed_entry_count INTEGER NOT NULL CHECK (exposed_entry_count >= 0),
  created_at TEXT NOT NULL,
  PRIMARY KEY (workspace_id, work_view_id)
);
	"#;

pub const CURRENT_SCHEMA_BATCHES: &[&str] = &[
    SCHEMA_CORE,
    SCHEMA_MERGE_PLUGIN_APPROVALS,
    SCHEMA_MATERIALIZATION,
    SCHEMA_PREPARATION,
    SCHEMA_ENV_SETUP_INDEXES,
    SCHEMA_WORK_VIEWS,
    SCHEMA_WORK_VIEW_ACCEPT_OPERATIONS,
];
