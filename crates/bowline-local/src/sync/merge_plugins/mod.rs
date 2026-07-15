use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use bowline_core::ids::{DeviceId, WorkspaceId};
use serde::{Deserialize, Serialize};

mod builtins;
mod config;
mod matcher;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod symlink_tests;
mod wasm;

pub(crate) use builtins::{
    built_in_merge_plugin_conflicts_by_default, structured_merge_output_is_valid,
};
pub use config::MergePluginConfigError;
use config::{ProjectMergePluginConfig, validate_project_relative_module_path};
use matcher::{MAX_GLOB_MATCH_BYTES, glob_matches, normalize_workspace_match_path};

const CONFIG_FILE_NAME: &str = ".bowlinemerge.toml";
const DIGEST_PREFIX: &str = "blake3:";
const MAX_PLUGIN_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PLUGIN_MATCH_PATH_BYTES: usize = MAX_GLOB_MATCH_BYTES;
const MAX_PLUGIN_MODULE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PLUGIN_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePluginIdentity {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub matcher_version: String,
    pub validator_version: String,
}

impl MergePluginIdentity {
    pub fn stable_key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.id, self.version, self.digest, self.matcher_version, self.validator_version
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePluginApprovalRecord {
    pub workspace_id: WorkspaceId,
    pub plugin: MergePluginIdentity,
    pub state: String,
    pub approved_by_device_id: DeviceId,
    pub approved_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePluginApprovalInput {
    pub workspace_id: WorkspaceId,
    pub plugin: MergePluginIdentity,
    pub approved_by_device_id: DeviceId,
    pub approved_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePluginAuditRecord {
    pub path: String,
    pub plugin: MergePluginIdentity,
    pub output_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePluginApprovalRequest {
    pub plugin: MergePluginIdentity,
    pub patterns: Vec<String>,
    pub module: String,
}

pub fn declared_approvable_merge_plugins(
    root: &Path,
) -> Result<Vec<MergePluginApprovalRequest>, MergePluginConfigError> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let text = fs::read_to_string(config_path).map_err(MergePluginConfigError::Io)?;
    let config = ProjectMergePluginConfig::parse(&text)?;
    let mut requests = Vec::new();
    for declaration in config.plugins {
        validate_project_relative_module_path(&declaration.module)?;
        if declaration.unsupported_matcher_contract().is_some() {
            continue;
        }
        requests.push(MergePluginApprovalRequest {
            plugin: declaration.identity(),
            patterns: declaration.patterns,
            module: declaration.module,
        });
    }
    Ok(requests)
}

#[derive(Debug)]
pub(crate) struct MergePluginRegistry {
    external: Vec<ExternalMergePlugin>,
    wasm_engine: Option<Arc<wasmtime::Engine>>,
    audit: Mutex<Vec<MergePluginAuditRecord>>,
}

impl MergePluginRegistry {
    pub(super) fn built_in() -> Self {
        Self {
            external: Vec::new(),
            wasm_engine: None,
            audit: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn load_project(
        root: &Path,
        workspace_id: &WorkspaceId,
        approvals: &[MergePluginApprovalRecord],
    ) -> Result<ProjectMergePluginRegistry, MergePluginConfigError> {
        let config_path = root.join(CONFIG_FILE_NAME);
        let config = match fs::read_to_string(&config_path) {
            Ok(text) => ProjectMergePluginConfig::parse(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProjectMergePluginRegistry {
                    registry: Self::built_in(),
                    approval_requests: Vec::new(),
                    config_path,
                });
            }
            Err(error) => return Err(MergePluginConfigError::Io(error)),
        };

        let approved = approvals
            .iter()
            .filter(|record| record.workspace_id == *workspace_id && record.state == "approved")
            .map(|record| record.plugin.stable_key())
            .collect::<BTreeSet<_>>();
        let mut external = Vec::with_capacity(config.plugins.len());
        let mut approval_requests = Vec::new();
        let has_approved_plugin = config.plugins.iter().any(|declaration| {
            declaration.unsupported_matcher_contract().is_none() && {
                let identity = declaration.identity();
                approved.contains(&identity.stable_key())
            }
        });
        let mut engine_error = None;
        let wasm_engine = if has_approved_plugin {
            match wasm::merge_plugin_engine() {
                Ok(engine) => Some(Arc::new(engine)),
                Err(error) => {
                    engine_error = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };

        for declaration in config.plugins {
            let identity = declaration.identity();
            let module_relative = validate_project_relative_module_path(&declaration.module)?;
            if let Some(version) = declaration
                .unsupported_matcher_contract()
                .map(str::to_string)
            {
                let uses_unknown_matcher_syntax = declaration
                    .patterns
                    .iter()
                    .any(|pattern| !pattern_uses_portable_contract2_tokens(pattern));
                external.push(ExternalMergePlugin {
                    identity,
                    patterns: declaration.patterns,
                    approved_on_device: false,
                    loaded: LoadedMergePlugin::UnsupportedMatcherContract {
                        version,
                        uses_unknown_matcher_syntax,
                    },
                });
                continue;
            }

            let approved_on_device = approved.contains(&identity.stable_key());
            if !approved_on_device {
                approval_requests.push(MergePluginApprovalRequest {
                    plugin: identity.clone(),
                    patterns: declaration.patterns.clone(),
                    module: declaration.module.clone(),
                });
            }
            let loaded = if approved_on_device {
                match (&wasm_engine, &engine_error) {
                    (Some(engine), _) => {
                        load_declared_module(root, &module_relative, &identity.digest, engine)
                    }
                    (None, Some(reason)) => {
                        LoadedMergePlugin::Unavailable(format!("WASM engine unavailable: {reason}"))
                    }
                    (None, None) => LoadedMergePlugin::Unavailable(
                        "WASM engine unavailable for approved plugin".to_string(),
                    ),
                }
            } else {
                LoadedMergePlugin::Unavailable("module is not approved".to_string())
            };
            external.push(ExternalMergePlugin {
                identity,
                patterns: declaration.patterns,
                approved_on_device,
                loaded,
            });
        }

        Ok(ProjectMergePluginRegistry {
            registry: Self {
                external,
                wasm_engine,
                audit: Mutex::new(Vec::new()),
            },
            approval_requests,
            config_path,
        })
    }

    pub(super) fn merge_external(
        &self,
        path: &str,
        base: &[u8],
        local: &[u8],
        remote: &[u8],
    ) -> ExternalMergeDecision {
        let normalized_path = normalize_workspace_match_path(path);
        if normalized_path.len() > MAX_PLUGIN_MATCH_PATH_BYTES {
            let possible_plugins = self
                .external
                .iter()
                .filter(|plugin| plugin.might_match_oversized_path(&normalized_path))
                .collect::<Vec<_>>();
            if possible_plugins.is_empty() {
                return ExternalMergeDecision::NoMatch;
            }
            let plugin_ids = possible_plugins
                .iter()
                .map(|plugin| plugin.identity.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return ExternalMergeDecision::Conflict(format!(
                "path `{path}` exceeds {MAX_PLUGIN_MATCH_PATH_BYTES} bytes for merge plugin matching: {plugin_ids}"
            ));
        }
        let matches = self
            .external
            .iter()
            .filter(|plugin| plugin.matches(&normalized_path))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return ExternalMergeDecision::NoMatch;
        }
        if matches.len() > 1 {
            let plugin_ids = matches
                .iter()
                .map(|plugin| plugin.identity.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return ExternalMergeDecision::Conflict(format!(
                "multiple merge plugins match `{path}`: {plugin_ids}"
            ));
        }

        let plugin = matches[0];
        if !plugin.approved_on_device
            && !matches!(
                plugin.loaded,
                LoadedMergePlugin::UnsupportedMatcherContract { .. }
            )
        {
            return ExternalMergeDecision::Conflict(format!(
                "merge plugin `{}` {} is not approved on this device",
                plugin.identity.id, plugin.identity.version
            ));
        }
        let loaded = match &plugin.loaded {
            LoadedMergePlugin::Ready(module) => module,
            LoadedMergePlugin::UnsupportedMatcherContract { version, .. } => {
                return ExternalMergeDecision::Conflict(format!(
                    "merge plugin `{}` unavailable: unsupported matcher contract `{version}`; use a Bowline version that supports this matcher contract",
                    plugin.identity.id,
                ));
            }
            LoadedMergePlugin::Unavailable(reason) => {
                return ExternalMergeDecision::Conflict(format!(
                    "merge plugin `{}` unavailable: {reason}",
                    plugin.identity.id
                ));
            }
        };
        let Some(engine) = &self.wasm_engine else {
            return ExternalMergeDecision::Conflict(format!(
                "merge plugin `{}` unavailable: WASM engine unavailable",
                plugin.identity.id
            ));
        };
        let input_len = base.len() + local.len() + remote.len();
        if input_len > MAX_PLUGIN_INPUT_BYTES {
            return ExternalMergeDecision::Conflict(format!(
                "merge plugin `{}` input exceeds {} bytes",
                plugin.identity.id, MAX_PLUGIN_INPUT_BYTES
            ));
        }

        match wasm::merge_with_wasm_plugin(
            engine,
            loaded,
            path,
            base,
            local,
            remote,
            wasm::WasmPluginLimits::default_with_output_limit(MAX_PLUGIN_OUTPUT_BYTES),
        ) {
            Ok(Some(bytes)) => {
                if !structured_merge_output_is_valid(path, &bytes) {
                    return ExternalMergeDecision::Conflict(format!(
                        "merge plugin `{}` produced invalid output for `{path}`",
                        plugin.identity.id
                    ));
                }
                let output_digest = blake3_digest(&bytes);
                self.push_audit(MergePluginAuditRecord {
                    path: path.to_string(),
                    plugin: plugin.identity.clone(),
                    output_digest,
                });
                ExternalMergeDecision::Merged(bytes)
            }
            Ok(None) => ExternalMergeDecision::Conflict(format!(
                "merge plugin `{}` declined automatic merge",
                plugin.identity.id
            )),
            Err(wasm::WasmMergeError::ComputeBudgetExhausted) => {
                ExternalMergeDecision::Conflict(format!(
                    "merge plugin `{}` exceeded its compute budget",
                    plugin.identity.id
                ))
            }
            Err(error) => ExternalMergeDecision::Conflict(format!(
                "merge plugin `{}` failed safely: {error}",
                plugin.identity.id
            )),
        }
    }

    pub(super) fn take_audit_records(&self) -> Vec<MergePluginAuditRecord> {
        self.audit
            .lock()
            // Audit records are append-only diagnostics; a poisoned writer
            // should not make the sync hot path fail closed.
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    fn push_audit(&self, record: MergePluginAuditRecord) {
        self.audit
            .lock()
            // Audit records are append-only diagnostics; poisoning only means
            // the previous diagnostic writer unwound.
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }
}

#[derive(Debug)]
pub(super) struct ProjectMergePluginRegistry {
    pub(super) registry: MergePluginRegistry,
    pub(super) approval_requests: Vec<MergePluginApprovalRequest>,
    pub(super) config_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExternalMergeDecision {
    NoMatch,
    Merged(Vec<u8>),
    Conflict(String),
}

#[derive(Debug)]
struct ExternalMergePlugin {
    identity: MergePluginIdentity,
    patterns: Vec<String>,
    approved_on_device: bool,
    loaded: LoadedMergePlugin,
}

impl ExternalMergePlugin {
    fn matches(&self, normalized_path: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            let normalized_pattern = normalize_workspace_match_path(pattern);
            match &self.loaded {
                LoadedMergePlugin::UnsupportedMatcherContract {
                    uses_unknown_matcher_syntax: true,
                    ..
                } => pattern_may_match_unknown_matcher_syntax(&normalized_pattern, normalized_path),
                LoadedMergePlugin::UnsupportedMatcherContract {
                    uses_unknown_matcher_syntax: false,
                    ..
                } => glob_matches(&normalized_pattern, normalized_path),
                LoadedMergePlugin::Ready(_) | LoadedMergePlugin::Unavailable(_) => {
                    glob_matches(&normalized_pattern, normalized_path)
                }
            }
        })
    }
    fn might_match_oversized_path(&self, normalized_path: &str) -> bool {
        if matches!(
            self.loaded,
            LoadedMergePlugin::UnsupportedMatcherContract {
                uses_unknown_matcher_syntax: true,
                ..
            }
        ) {
            return self.patterns.iter().any(|pattern| {
                let normalized_pattern = normalize_workspace_match_path(pattern);
                pattern_may_match_unknown_matcher_syntax(&normalized_pattern, normalized_path)
            });
        }
        self.patterns.iter().any(|pattern| {
            let normalized_pattern = normalize_workspace_match_path(pattern);
            pattern_may_match_oversized_path(&normalized_pattern, normalized_path)
        })
    }
}
fn pattern_may_match_oversized_path(pattern: &str, normalized_path: &str) -> bool {
    let Some(first_wildcard) = pattern.find(['*', '?']) else {
        return pattern == normalized_path;
    };
    let Some(last_wildcard) = pattern.rfind(['*', '?']) else {
        return pattern == normalized_path;
    };
    let prefix = &pattern[..first_wildcard];
    let suffix = &pattern[last_wildcard + 1..];
    (prefix.is_empty() || normalized_path.starts_with(prefix))
        && (suffix.is_empty() || normalized_path.ends_with(suffix))
}
fn pattern_may_match_unknown_matcher_syntax(pattern: &str, normalized_path: &str) -> bool {
    let Some(first_token) = pattern.find(is_future_matcher_token) else {
        return pattern == normalized_path;
    };
    let Some(last_token) = pattern.rfind(is_future_matcher_token) else {
        return pattern == normalized_path;
    };
    let prefix = &pattern[..first_token];
    let suffix = &pattern[last_token + 1..];
    (prefix.is_empty() || normalized_path.starts_with(prefix))
        && (suffix.is_empty() || normalized_path.ends_with(suffix))
}
fn is_future_matcher_token(character: char) -> bool {
    matches!(
        character,
        '*' | '?' | '{' | '}' | '[' | ']' | '(' | ')' | '!' | '|' | '+' | '@'
    )
}

fn pattern_uses_portable_contract2_tokens(pattern: &str) -> bool {
    let normalized = normalize_workspace_match_path(pattern);
    normalized.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'*' | b'?')
    })
}

#[derive(Debug)]
enum LoadedMergePlugin {
    Ready(Arc<wasmtime::Module>),
    UnsupportedMatcherContract {
        version: String,
        uses_unknown_matcher_syntax: bool,
    },
    Unavailable(String),
}

fn load_declared_module(
    root: &Path,
    relative_path: &Path,
    expected_digest: &str,
    engine: &wasmtime::Engine,
) -> LoadedMergePlugin {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => return LoadedMergePlugin::Unavailable(error.to_string()),
    };
    let path = root.join(relative_path);
    let canonical_path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedMergePlugin::Unavailable("module file is missing".to_string());
        }
        Err(error) => return LoadedMergePlugin::Unavailable(error.to_string()),
    };
    if !canonical_path.starts_with(&root) {
        return LoadedMergePlugin::Unavailable(
            "module path resolves outside workspace".to_string(),
        );
    }

    let file = match fs::File::open(&canonical_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedMergePlugin::Unavailable("module file is missing".to_string());
        }
        Err(error) => return LoadedMergePlugin::Unavailable(error.to_string()),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return LoadedMergePlugin::Unavailable(error.to_string()),
    };
    if !metadata.is_file() {
        return LoadedMergePlugin::Unavailable("module path is not a regular file".to_string());
    }
    if metadata.len() > MAX_PLUGIN_MODULE_BYTES as u64 {
        return LoadedMergePlugin::Unavailable(format!(
            "module is larger than {MAX_PLUGIN_MODULE_BYTES} bytes"
        ));
    }

    let mut reader = file.take(MAX_PLUGIN_MODULE_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    if let Err(error) = reader.read_to_end(&mut bytes) {
        return LoadedMergePlugin::Unavailable(error.to_string());
    }
    if bytes.len() > MAX_PLUGIN_MODULE_BYTES {
        return LoadedMergePlugin::Unavailable(format!(
            "module is larger than {MAX_PLUGIN_MODULE_BYTES} bytes"
        ));
    }

    let actual_digest = blake3_digest(&bytes);
    if expected_digest != actual_digest {
        return LoadedMergePlugin::Unavailable(format!(
            "digest mismatch, expected {expected_digest}, found {actual_digest}"
        ));
    }
    match wasm::compile_merge_plugin_module(engine, &bytes) {
        Ok(module) => LoadedMergePlugin::Ready(Arc::new(module)),
        Err(error) => LoadedMergePlugin::Unavailable(error.to_string()),
    }
}

fn blake3_digest(bytes: &[u8]) -> String {
    format!("{DIGEST_PREFIX}{}", blake3::hash(bytes).to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sync::merge_plugins::matcher::policy_bound_matcher_version, workspace::TempWorkspace,
    };

    #[test]
    fn unapproved_project_plugin_conflicts_instead_of_running() {
        let config = ProjectMergePluginConfig::parse(
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
"#,
        )
        .expect("config parses");
        let declaration = &config.plugins[0];
        let registry = MergePluginRegistry {
            external: vec![ExternalMergePlugin {
                identity: declaration.identity(),
                patterns: declaration.patterns.clone(),
                approved_on_device: false,
                loaded: LoadedMergePlugin::Unavailable("module file is missing".to_string()),
            }],
            wasm_engine: None,
            audit: Mutex::new(Vec::new()),
        };

        match registry.merge_external("analysis.ipynb", b"{}", b"{}", b"{}") {
            ExternalMergeDecision::Conflict(reason) => {
                assert!(reason.contains("not approved"));
            }
            decision => panic!("expected conflict, got {decision:?}"),
        }
    }

    #[test]
    fn stale_matcher_contract_disables_only_that_plugin() {
        let workspace =
            TempWorkspace::new("merge-plugin-stale-matcher-contract").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
matcher-version = "1"

[[plugins]]
id = "json"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/json.wasm"
match = ["*.json"]
"#,
        )
        .expect("write config");

        let workspace_id = WorkspaceId::new("ws_plugins");
        let plugins = MergePluginRegistry::load_project(workspace.root(), &workspace_id, &[])
            .expect("project registry loads");

        assert_eq!(plugins.approval_requests.len(), 1);
        assert_eq!(plugins.approval_requests[0].plugin.id, "json");
        match plugins
            .registry
            .merge_external("analysis.ipynb", b"{}", b"{}", b"{}")
        {
            ExternalMergeDecision::Conflict(reason) => {
                assert!(reason.contains("unsupported matcher contract `1`"));
            }
            decision => panic!("expected conflict, got {decision:?}"),
        }
        match plugins
            .registry
            .merge_external("vendored/dep/run.ipynb", b"{}", b"{}", b"{}")
        {
            ExternalMergeDecision::NoMatch => {}
            decision => panic!("expected no match, got {decision:?}"),
        }
    }

    #[test]
    fn declared_approvable_merge_plugins_filters_unsupported_contracts() {
        let workspace =
            TempWorkspace::new("merge-plugin-approvable-declarations").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
matcher-version = "1"

[[plugins]]
id = "json"
version = "1.0.0"
digest = "blake3:1234"
module = ".bowline/plugins/json.wasm"
match = ["*.json"]
matcher-version = "2"
validator-version = "1"
"#,
        )
        .expect("write config");

        let requests =
            declared_approvable_merge_plugins(workspace.root()).expect("declarations load");

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].plugin.id, "json");
        assert_eq!(
            requests[0].plugin.matcher_version,
            policy_bound_matcher_version("2", &["*.json".to_string()])
        );
        assert_eq!(requests[0].plugin.validator_version, "1");
        assert_eq!(requests[0].module, ".bowline/plugins/json.wasm");
    }

    #[test]
    fn stale_matcher_contract_still_validates_module_path() {
        let workspace = TempWorkspace::new("merge-plugin-stale-invalid-module").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = "../../evil.wasm"
match = ["*.ipynb"]
matcher-version = "1"
"#,
        )
        .expect("write config");

        let workspace_id = WorkspaceId::new("ws_plugins");
        let result = MergePluginRegistry::load_project(workspace.root(), &workspace_id, &[]);

        assert!(matches!(
            result,
            Err(MergePluginConfigError::UnsafeModulePath(_))
        ));
    }

    #[test]
    fn future_matcher_contracts_do_not_downgrade_message() {
        let workspace = TempWorkspace::new("merge-plugin-future-matcher").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
matcher-version = "3"
"#,
        )
        .expect("write config");

        let workspace_id = WorkspaceId::new("ws_plugins");
        let plugins = MergePluginRegistry::load_project(workspace.root(), &workspace_id, &[])
            .expect("project registry loads");

        match plugins
            .registry
            .merge_external("analysis.ipynb", b"{}", b"{}", b"{}")
        {
            ExternalMergeDecision::Conflict(reason) => {
                assert!(reason.contains("unsupported matcher contract `3`"));
                assert!(reason.contains("supports this matcher contract"));
                assert!(!reason.contains("update matcher-version to `2`"));
            }
            decision => panic!("expected conflict, got {decision:?}"),
        }
        assert!(matches!(
            plugins
                .registry
                .merge_external("notes.txt", b"base", b"local", b"remote"),
            ExternalMergeDecision::NoMatch
        ));
    }

    #[test]
    fn stale_matcher_contract_approval_does_not_construct_wasm_engine() {
        let workspace = TempWorkspace::new("merge-plugin-stale-approved-only").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
matcher-version = "1"
"#,
        )
        .expect("write config");
        let workspace_id = WorkspaceId::new("ws_plugins");
        let registry = MergePluginRegistry::load_project(
            workspace.root(),
            &workspace_id,
            &[MergePluginApprovalRecord {
                workspace_id: workspace_id.clone(),
                plugin: MergePluginIdentity {
                    id: "notebooks".to_string(),
                    version: "1.0.0".to_string(),
                    digest: "blake3:abcd".to_string(),
                    matcher_version: policy_bound_matcher_version("1", &["*.ipynb".to_string()]),
                    validator_version: "1".to_string(),
                },
                state: "approved".to_string(),
                approved_by_device_id: DeviceId::new("device_local"),
                approved_at: "2026-07-02T10:00:00Z".to_string(),
            }],
        )
        .expect("project registry loads");

        assert!(registry.registry.wasm_engine.is_none());
    }

    #[test]
    fn unapproved_project_plugin_does_not_read_declared_module() {
        let workspace = TempWorkspace::new("merge-plugin-unapproved").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/missing.wasm"
match = ["*.ipynb"]
"#,
        )
        .expect("config");

        let registry =
            MergePluginRegistry::load_project(workspace.root(), &WorkspaceId::new("ws_code"), &[])
                .expect("registry");

        assert_eq!(registry.approval_requests.len(), 1);
        match registry
            .registry
            .merge_external("analysis.ipynb", b"{}", b"{}", b"{}")
        {
            ExternalMergeDecision::Conflict(reason) => {
                assert!(reason.contains("not approved"));
            }
            decision => panic!("expected not approved conflict, got {decision:?}"),
        }
    }

    #[test]
    fn project_plugin_pattern_change_requires_new_approval() {
        let workspace = TempWorkspace::new("merge-plugin-pattern-change").expect("workspace");
        fs::write(
            workspace.root().join(CONFIG_FILE_NAME),
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/missing.wasm"
match = ["*.json"]
"#,
        )
        .expect("config");
        let old_policy = MergePluginIdentity {
            id: "notebooks".to_string(),
            version: "1.0.0".to_string(),
            digest: "blake3:abcd".to_string(),
            matcher_version: policy_bound_matcher_version("2", &["*.ipynb".to_string()]),
            validator_version: "1".to_string(),
        };

        let registry = MergePluginRegistry::load_project(
            workspace.root(),
            &WorkspaceId::new("ws_code"),
            &[MergePluginApprovalRecord {
                workspace_id: WorkspaceId::new("ws_code"),
                plugin: old_policy,
                state: "approved".to_string(),
                approved_by_device_id: DeviceId::new("device_local"),
                approved_at: "2026-07-02T10:00:00Z".to_string(),
            }],
        )
        .expect("registry");

        assert_eq!(registry.approval_requests.len(), 1);
        match registry
            .registry
            .merge_external("package.json", b"{}", b"{}", b"{}")
        {
            ExternalMergeDecision::Conflict(reason) => {
                assert!(reason.contains("not approved"));
            }
            decision => panic!("expected not approved conflict, got {decision:?}"),
        }
    }
}
