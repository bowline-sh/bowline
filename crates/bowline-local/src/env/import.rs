use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bowline_core::{
    ids::{EnvRecordId, WorkspaceId},
    policy::{AccessFlag, PathClassification},
};
use bowline_storage::{
    EnvelopeContext, EnvelopeError, ObjectKind, StorageKey, seal, workspace_id_hash,
};
use serde::Serialize;

use crate::{
    metadata::{EnvRecord, EnvRecordSourceReplacement, MetadataError, MetadataStore},
    scanner::{ObservationWriteScope, ScanReport},
};

use super::parser::{EnvLineKind, ParsedEnvFile, parse_env_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvImportReport {
    pub imported_file_count: usize,
    pub imported_record_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedEnvImport {
    pub(crate) report: EnvImportReport,
    replacements: Vec<EnvRecordSourceReplacement>,
}

#[derive(Debug)]
pub enum EnvImportError {
    Io { path: PathBuf, source: io::Error },
    Metadata(MetadataError),
    Json(serde_json::Error),
    Envelope(EnvelopeError),
}

pub fn import_env_records_from_scan(
    store: &mut MetadataStore,
    workspace_id: &WorkspaceId,
    workspace_root: &Path,
    report: &ScanReport,
    workspace_content_key: Option<[u8; 32]>,
    now: &str,
) -> Result<EnvImportReport, EnvImportError> {
    let prepared = prepare_env_records_from_scan(
        store,
        workspace_id,
        workspace_root,
        report,
        workspace_content_key,
        ObservationWriteScope::Full,
        now,
    )?;
    let report = prepared.report.clone();
    store.commit_env_record_replacements(workspace_id, &prepared.replacements)?;
    Ok(report)
}

/// Prepare env-record replacements for `report`. `write_scope` bounds which
/// existing env sources this scan may prune: a partial scan only observed the
/// env files its scope owns, so it must never treat an env source outside its
/// scope as "gone" and blank it out (KTD-13). A full scan owns every source.
pub(crate) fn prepare_env_records_from_scan(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    workspace_root: &Path,
    report: &ScanReport,
    workspace_content_key: Option<[u8; 32]>,
    write_scope: ObservationWriteScope<'_>,
    now: &str,
) -> Result<PreparedEnvImport, EnvImportError> {
    let mut imported_file_count = 0;
    let mut imported_record_count = 0;
    let mut replacements = Vec::new();
    let current_env_sources = report
        .paths
        .iter()
        .filter(|observed| {
            !observed.is_dir
                && !observed.is_symlink
                && observed.policy.classification == PathClassification::ProjectEnv
        })
        .map(|observed| observed.path.clone())
        .collect::<BTreeSet<_>>();
    let stale_sources = store
        .env_records(workspace_id)?
        .into_iter()
        .map(|record| record.source_path)
        // Only prune sources this scan owns; a source outside the scope was not
        // observed this tick and its absence from the partial report is not
        // authoritative.
        .filter(|source| write_scope.owns_path(source))
        .filter(|source| !current_env_sources.contains(source))
        .collect::<BTreeSet<_>>();
    for source in stale_sources {
        replacements.push(EnvRecordSourceReplacement::new(source, Vec::new()));
    }

    for observed in &report.paths {
        if observed.is_dir
            || observed.is_symlink
            || observed.policy.classification != PathClassification::ProjectEnv
        {
            continue;
        }

        let bytes =
            fs::read(workspace_root.join(&observed.path)).map_err(|source| EnvImportError::Io {
                path: workspace_root.join(&observed.path),
                source,
            })?;
        let parsed = parse_env_text(&observed.path, profile_for_env_path(&observed.path), &bytes);
        let records = records_for_parsed_env(
            workspace_id,
            observed.project_id.clone(),
            &parsed,
            workspace_content_key,
            now,
        )?;
        imported_record_count += records.len();
        imported_file_count += 1;
        replacements.push(EnvRecordSourceReplacement::new(
            observed.path.clone(),
            records,
        ));
    }

    Ok(PreparedEnvImport {
        report: EnvImportReport {
            imported_file_count,
            imported_record_count,
        },
        replacements,
    })
}

pub(crate) fn records_for_parsed_env(
    workspace_id: &WorkspaceId,
    project_id: Option<bowline_core::ids::ProjectId>,
    parsed: &ParsedEnvFile,
    workspace_content_key: Option<[u8; 32]>,
    now: &str,
) -> Result<Vec<EnvRecord>, EnvImportError> {
    let mut records = Vec::new();
    let mut mark_next_key_machine_local = false;
    for line in &parsed.lines {
        match &line.kind {
            EnvLineKind::KeyValue(value) => {
                let id = env_record_id(
                    workspace_id,
                    &parsed.source_path,
                    &value.key,
                    value.occurrence_index,
                );
                let value_ciphertext_ref = seal_env_record_value(
                    workspace_id,
                    &parsed.source_path,
                    &id,
                    1,
                    value.value.as_bytes(),
                    workspace_content_key,
                )?;
                records.push(EnvRecord {
                    id,
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    source_path: parsed.source_path.clone(),
                    profile: parsed.profile.clone(),
                    key_name: value.key.clone(),
                    occurrence_index: u32::try_from(value.occurrence_index).unwrap_or(u32::MAX),
                    line_kind: "key-value".to_string(),
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    value_ciphertext_ref,
                    encrypted_locator_json: locator_json(
                        workspace_id,
                        &parsed.source_path,
                        &value.key,
                        value.occurrence_index,
                    )?,
                    format_json: serde_json::to_string(&EnvFormatMetadata {
                        source_line: line.line_number,
                        export_prefix: value.export_prefix,
                        quote_style: format!("{:?}", value.quote_style).to_ascii_lowercase(),
                    })?,
                    materialization_state: materialization_state_for_key(workspace_content_key),
                    restriction_state: if mark_next_key_machine_local {
                        "machine-local".to_string()
                    } else {
                        "unrestricted".to_string()
                    },
                    key_epoch: 1,
                    metadata_json: if mark_next_key_machine_local {
                        "{\"redacted\":true,\"machineLocal\":true}".to_string()
                    } else {
                        "{\"redacted\":true}".to_string()
                    },
                    updated_at: now.to_string(),
                });
                mark_next_key_machine_local = false;
            }
            EnvLineKind::Opaque(_) => {
                let key_name = format!("__opaque_line_{}", line.ordinal);
                let id = env_record_id(workspace_id, &parsed.source_path, &key_name, 0);
                let value_ciphertext_ref = match &line.kind {
                    EnvLineKind::Opaque(opaque) => seal_env_record_value(
                        workspace_id,
                        &parsed.source_path,
                        &id,
                        1,
                        opaque.bytes.as_bytes(),
                        workspace_content_key,
                    )?,
                    EnvLineKind::Blank | EnvLineKind::Comment | EnvLineKind::KeyValue(_) => None,
                };
                records.push(EnvRecord {
                    id,
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    source_path: parsed.source_path.clone(),
                    profile: parsed.profile.clone(),
                    key_name: key_name.clone(),
                    occurrence_index: u32::try_from(line.ordinal).unwrap_or(u32::MAX),
                    line_kind: "opaque".to_string(),
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
                    value_ciphertext_ref,
                    encrypted_locator_json: locator_json(
                        workspace_id,
                        &parsed.source_path,
                        &key_name,
                        0,
                    )?,
                    format_json: serde_json::to_string(&EnvFormatMetadata {
                        source_line: line.line_number,
                        export_prefix: false,
                        quote_style: "opaque".to_string(),
                    })?,
                    materialization_state: materialization_state_for_key(workspace_content_key),
                    restriction_state: "unrestricted".to_string(),
                    key_epoch: 1,
                    metadata_json: "{\"redacted\":true}".to_string(),
                    updated_at: now.to_string(),
                });
                // Opaque lines break adjacency: a machine-local marker only
                // applies to the immediately following key-value line.
                mark_next_key_machine_local = false;
            }
            EnvLineKind::Blank => {
                mark_next_key_machine_local = false;
            }
            EnvLineKind::Comment => {
                mark_next_key_machine_local = is_machine_local_marker(&line.raw);
            }
        }
    }
    Ok(records)
}

fn is_machine_local_marker(raw: &[u8]) -> bool {
    std::str::from_utf8(raw)
        .ok()
        .is_some_and(|line| line.trim() == "# bowline:machine-local")
}

const ENV_VALUE_ENVELOPE_PREFIX: &str = "env-envelope-v1:";
const ENV_VALUE_FORMAT_VERSION: u16 = 1;
const ENV_VALUE_KEY_CONTEXT: &str = "bowline env value v1";

fn materialization_state_for_key(workspace_content_key: Option<[u8; 32]>) -> String {
    if workspace_content_key.is_some() {
        "materialized".to_string()
    } else {
        "pending".to_string()
    }
}

fn seal_env_record_value(
    workspace_id: &WorkspaceId,
    source_path: &str,
    record_id: &EnvRecordId,
    key_epoch: u32,
    plaintext: &[u8],
    workspace_content_key: Option<[u8; 32]>,
) -> Result<Option<String>, EnvImportError> {
    let Some(workspace_content_key) = workspace_content_key else {
        return Ok(None);
    };
    let envelope = seal(
        plaintext,
        env_value_storage_key(workspace_content_key),
        &env_value_context(workspace_id, source_path, record_id, key_epoch),
    )?;
    Ok(Some(format!(
        "{ENV_VALUE_ENVELOPE_PREFIX}{}",
        STANDARD.encode(envelope.as_bytes())
    )))
}

fn env_value_storage_key(workspace_content_key: [u8; 32]) -> StorageKey {
    StorageKey::from_bytes(blake3::derive_key(
        ENV_VALUE_KEY_CONTEXT,
        &workspace_content_key,
    ))
}

fn env_value_context(
    workspace_id: &WorkspaceId,
    source_path: &str,
    record_id: &EnvRecordId,
    key_epoch: u32,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::WorkspaceFileV1,
        object_id: format!(
            "env-value:{}",
            blake3::hash(source_path.as_bytes()).to_hex()
        ),
        record_id: record_id.as_str().to_string(),
        key_epoch,
        format_version: ENV_VALUE_FORMAT_VERSION,
    }
}

fn locator_json(
    workspace_id: &WorkspaceId,
    source_path: &str,
    key_name: &str,
    occurrence_index: usize,
) -> Result<String, serde_json::Error> {
    let associated_data = format!(
        "{}:{}:{}:{}",
        workspace_id.as_str(),
        blake3::hash(source_path.as_bytes()).to_hex(),
        blake3::hash(key_name.as_bytes()).to_hex(),
        occurrence_index
    );
    let associated_data_hash = format!("b3_{}", blake3::hash(associated_data.as_bytes()).to_hex());
    serde_json::to_string(&EnvLocalMetadataLocator {
        storage: "source-pack-file",
        associated_data_hash: &associated_data_hash,
        key_epoch: 1,
        redacted: true,
    })
}

fn env_record_id(
    workspace_id: &WorkspaceId,
    source_path: &str,
    key_name: &str,
    occurrence_index: usize,
) -> EnvRecordId {
    let input = format!(
        "{}\0{}\0{}\0{}",
        workspace_id.as_str(),
        source_path,
        key_name,
        occurrence_index
    );
    EnvRecordId::new(format!("env_{}", blake3::hash(input.as_bytes()).to_hex()))
}

fn profile_for_env_path(path: &str) -> String {
    let Some(name) = path.rsplit('/').next() else {
        return "default".to_string();
    };
    match name {
        ".env" => "default".to_string(),
        ".env.local" => "local".to_string(),
        _ => {
            // Trim dots before the empty check so ".env.." / "..env" fall back
            // to "default" instead of producing an empty profile name.
            let profile = name
                .strip_prefix(".env.")
                .or_else(|| name.strip_suffix(".env"))
                .unwrap_or("default")
                .trim_matches('.');
            if profile.is_empty() {
                "default".to_string()
            } else {
                profile.to_string()
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvLocalMetadataLocator<'a> {
    storage: &'a str,
    associated_data_hash: &'a str,
    key_epoch: u32,
    redacted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvFormatMetadata {
    source_line: usize,
    export_prefix: bool,
    quote_style: String,
}

impl fmt::Display for EnvImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "env import failed for {}: {source}",
                    path.display()
                )
            }
            Self::Metadata(error) => error.fmt(formatter),
            Self::Json(error) => write!(formatter, "env import JSON failed: {error}"),
            Self::Envelope(error) => write!(formatter, "env import encryption failed: {error}"),
        }
    }
}

impl Error for EnvImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Metadata(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Envelope(error) => Some(error),
        }
    }
}

impl From<MetadataError> for EnvImportError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<EnvelopeError> for EnvImportError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

impl From<serde_json::Error> for EnvImportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use bowline_core::ids::{ProjectId, WorkspaceId};
    use bowline_core::{status::ObservedWorkspaceSummary, workspace_graph::FileExecutability};
    use serde_json::Value;

    use super::*;
    use crate::{
        metadata::DEFAULT_DATABASE_FILE,
        policy::classify_path_with_builtin_policy,
        scanner::{PathObservation, ScanReport},
        workspace::TempWorkspace,
    };

    #[test]
    fn partial_env_import_preserves_sources_outside_scope() {
        let (temp, mut store, workspace_id) = env_store("env-partial-preserve");
        fs::write(temp.root().join(".env"), b"ROOT=1\n").expect("root env");
        fs::create_dir_all(temp.root().join("app")).expect("app dir");
        fs::write(temp.root().join("app/.env"), b"APP=1\n").expect("deep env");

        import_scoped(
            &mut store,
            &workspace_id,
            temp.root(),
            &env_report(temp.root(), &[".env", "app/.env"]),
            ObservationWriteScope::Full,
        );
        assert_eq!(
            env_source_paths(&store, &workspace_id),
            vec![".env", "app/.env"]
        );

        // A root-shallow tick observes only the root `.env`; the deep `app/.env`
        // is outside `RootLevelOnly` scope and must be preserved, not blanked out.
        import_scoped(
            &mut store,
            &workspace_id,
            temp.root(),
            &env_report(temp.root(), &[".env"]),
            ObservationWriteScope::RootLevelOnly,
        );
        assert_eq!(
            env_source_paths(&store, &workspace_id),
            vec![".env", "app/.env"]
        );
    }

    #[test]
    fn full_env_import_prunes_sources_the_report_no_longer_lists() {
        let (temp, mut store, workspace_id) = env_store("env-full-prune");
        fs::write(temp.root().join(".env"), b"ROOT=1\n").expect("root env");
        fs::create_dir_all(temp.root().join("app")).expect("app dir");
        fs::write(temp.root().join("app/.env"), b"APP=1\n").expect("deep env");

        import_scoped(
            &mut store,
            &workspace_id,
            temp.root(),
            &env_report(temp.root(), &[".env", "app/.env"]),
            ObservationWriteScope::Full,
        );

        // A full scan owns every source, so `app/.env` missing from the report is
        // authoritative and its records are pruned — the contrast that proves the
        // write scope drives pruning.
        import_scoped(
            &mut store,
            &workspace_id,
            temp.root(),
            &env_report(temp.root(), &[".env"]),
            ObservationWriteScope::Full,
        );
        assert_eq!(env_source_paths(&store, &workspace_id), vec![".env"]);
    }

    fn env_store(label: &str) -> (TempWorkspace, MetadataStore, WorkspaceId) {
        let temp = TempWorkspace::new(label).expect("temp workspace");
        let store = MetadataStore::open(temp.root().join(".state").join(DEFAULT_DATABASE_FILE))
            .expect("store");
        let workspace_id = WorkspaceId::new("ws_env_scope");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-06T00:00:00Z")
            .expect("workspace");
        (temp, store, workspace_id)
    }

    fn import_scoped(
        store: &mut MetadataStore,
        workspace_id: &WorkspaceId,
        root: &Path,
        report: &ScanReport,
        scope: ObservationWriteScope<'_>,
    ) {
        let prepared = prepare_env_records_from_scan(
            store,
            workspace_id,
            root,
            report,
            None,
            scope,
            "2026-07-06T00:01:00Z",
        )
        .expect("prepare env");
        store
            .commit_env_record_replacements(workspace_id, &prepared.replacements)
            .expect("apply env");
    }

    fn env_source_paths(store: &MetadataStore, workspace_id: &WorkspaceId) -> Vec<String> {
        let mut sources = store
            .env_records(workspace_id)
            .expect("env records")
            .into_iter()
            .map(|record| record.source_path)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        sources.sort();
        sources
    }

    fn env_report(root: &Path, env_paths: &[&str]) -> ScanReport {
        ScanReport {
            root: root.to_path_buf(),
            projects: Vec::new(),
            paths: env_paths
                .iter()
                .map(|path| PathObservation {
                    path: (*path).to_string(),
                    project_id: None,
                    is_dir: false,
                    is_symlink: false,
                    byte_len: Some(8),
                    stat: None,
                    executability: FileExecutability::Regular,
                    policy: classify_path_with_builtin_policy(*path),
                })
                .collect(),
            summary: ObservedWorkspaceSummary::default(),
        }
    }

    #[test]
    fn profiles_are_derived_from_env_file_names() {
        assert_eq!(profile_for_env_path(".env"), "default");
        assert_eq!(profile_for_env_path(".env.local"), "local");
        assert_eq!(profile_for_env_path(".env.development"), "development");
        assert_eq!(profile_for_env_path(".env.production"), "production");
        assert_eq!(profile_for_env_path("sub/dir/.env"), "default");
        assert_eq!(profile_for_env_path(".env.."), "default");
        assert_eq!(profile_for_env_path("..env"), "default");
        assert_eq!(profile_for_env_path(".env..."), "default");
        assert_eq!(profile_for_env_path(".env."), "default");
    }

    #[test]
    fn env_record_ids_are_stable_and_include_profile_key_and_occurrence() {
        let workspace = WorkspaceId::new("ws_test");
        let first = env_record_id(&workspace, ".env", "KEY", 0);

        assert_eq!(first, env_record_id(&workspace, ".env", "KEY", 0));
        assert_ne!(first, env_record_id(&workspace, ".env.local", "KEY", 0));
        assert_ne!(first, env_record_id(&workspace, ".env", "OTHER", 0));
        assert_ne!(first, env_record_id(&workspace, ".env", "KEY", 1));
    }

    #[test]
    fn records_for_parsed_env_keeps_key_and_opaque_metadata() {
        let workspace = WorkspaceId::new("ws_env");
        let parsed = parse_env_text(".env.local", "local", b"KEY=placeholder\nnot a kv\n");
        let records = records_for_parsed_env(
            &workspace,
            Some(ProjectId::new("project_app")),
            &parsed,
            None,
            "2026-07-01T00:00:00Z",
        )
        .expect("records");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key_name, "KEY");
        assert_eq!(records[0].profile, "local");
        assert_eq!(records[0].line_kind, "key-value");
        assert_eq!(records[0].occurrence_index, 0);
        assert_eq!(records[1].key_name, "__opaque_line_1");
        assert_eq!(records[1].line_kind, "opaque");
        let locator: Value = serde_json::from_str(&records[0].encrypted_locator_json).unwrap();
        assert_eq!(locator["storage"], "source-pack-file");
        assert_eq!(locator["redacted"], true);
    }

    #[test]
    fn records_for_parsed_env_seals_values_without_plaintext_storage() {
        let workspace = WorkspaceId::new("ws_env");
        let parsed = parse_env_text(
            "app/.env.local",
            "local",
            b"KEY=placeholder-a\nopaque placeholder-b\n",
        );
        let workspace_content_key = [71_u8; 32];
        let records = records_for_parsed_env(
            &workspace,
            Some(ProjectId::new("project_app")),
            &parsed,
            Some(workspace_content_key),
            "2026-07-01T00:00:00Z",
        )
        .expect("records");

        let key_record = records
            .iter()
            .find(|record| record.key_name == "KEY")
            .expect("key record");
        let encoded = key_record
            .value_ciphertext_ref
            .as_deref()
            .expect("sealed value")
            .strip_prefix(ENV_VALUE_ENVELOPE_PREFIX)
            .expect("envelope prefix");
        assert!(!encoded.contains("placeholder-a"));
        let sealed = STANDARD.decode(encoded).expect("base64 envelope");
        assert!(
            !sealed
                .windows(b"placeholder-a".len())
                .any(|window| window == b"placeholder-a")
        );

        let opened = bowline_storage::open(
            &sealed,
            env_value_storage_key(workspace_content_key),
            &env_value_context(
                &workspace,
                &key_record.source_path,
                &key_record.id,
                key_record.key_epoch,
            ),
        )
        .expect("envelope opens");
        assert_eq!(opened, b"placeholder-a");

        let opaque_record = records
            .iter()
            .find(|record| record.line_kind == "opaque")
            .expect("opaque record");
        let opaque_ref = opaque_record
            .value_ciphertext_ref
            .as_deref()
            .expect("opaque sealed");
        assert!(!opaque_ref.contains("placeholder-b"));
    }
}
