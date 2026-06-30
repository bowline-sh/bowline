use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    ids::{EnvRecordId, WorkspaceId},
    policy::{AccessFlag, PathClassification},
};
use serde::Serialize;

use crate::{
    metadata::{EnvRecord, MetadataError, MetadataStore},
    scanner::ScanReport,
};

use super::parser::{EnvLineKind, ParsedEnvFile, parse_env_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvImportReport {
    pub imported_file_count: usize,
    pub imported_record_count: usize,
}

#[derive(Debug)]
pub enum EnvImportError {
    Io { path: PathBuf, source: io::Error },
    Metadata(MetadataError),
    Json(serde_json::Error),
}

pub fn import_env_records_from_scan(
    store: &mut MetadataStore,
    workspace_id: &WorkspaceId,
    workspace_root: &Path,
    report: &ScanReport,
    _workspace_content_key: [u8; 32],
    now: &str,
) -> Result<EnvImportReport, EnvImportError> {
    let mut imported_file_count = 0;
    let mut imported_record_count = 0;
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
        .filter(|source| !current_env_sources.contains(source))
        .collect::<BTreeSet<_>>();
    for source in stale_sources {
        store.replace_env_records_for_source(workspace_id, &source, &[])?;
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
        let records =
            records_for_parsed_env(workspace_id, observed.project_id.clone(), &parsed, now)?;
        imported_record_count += records.len();
        imported_file_count += 1;
        store.replace_env_records_for_source(workspace_id, &observed.path, &records)?;
    }

    Ok(EnvImportReport {
        imported_file_count,
        imported_record_count,
    })
}

fn records_for_parsed_env(
    workspace_id: &WorkspaceId,
    project_id: Option<bowline_core::ids::ProjectId>,
    parsed: &ParsedEnvFile,
    now: &str,
) -> Result<Vec<EnvRecord>, EnvImportError> {
    let mut records = Vec::new();
    for line in &parsed.lines {
        match &line.kind {
            EnvLineKind::KeyValue(value) => {
                records.push(EnvRecord {
                    id: env_record_id(
                        workspace_id,
                        &parsed.source_path,
                        &value.key,
                        value.occurrence_index,
                    ),
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    source_path: parsed.source_path.clone(),
                    profile: parsed.profile.clone(),
                    key_name: value.key.clone(),
                    occurrence_index: u32::try_from(value.occurrence_index).unwrap_or(u32::MAX),
                    line_kind: "key-value".to_string(),
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
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
                    materialization_state: "materialized".to_string(),
                    restriction_state: "unrestricted".to_string(),
                    key_epoch: 1,
                    metadata_json: "{\"redacted\":true}".to_string(),
                    updated_at: now.to_string(),
                });
            }
            EnvLineKind::Opaque(_) => {
                let key_name = format!("__opaque_line_{}", line.ordinal);
                records.push(EnvRecord {
                    id: env_record_id(workspace_id, &parsed.source_path, &key_name, 0),
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    source_path: parsed.source_path.clone(),
                    profile: parsed.profile.clone(),
                    key_name: key_name.clone(),
                    occurrence_index: u32::try_from(line.ordinal).unwrap_or(u32::MAX),
                    line_kind: "opaque".to_string(),
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
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
                    materialization_state: "materialized".to_string(),
                    restriction_state: "unrestricted".to_string(),
                    key_epoch: 1,
                    metadata_json: "{\"redacted\":true}".to_string(),
                    updated_at: now.to_string(),
                });
            }
            EnvLineKind::Blank | EnvLineKind::Comment => {}
        }
    }
    Ok(records)
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
        _ => name
            .strip_prefix(".env.")
            .or_else(|| name.strip_suffix(".env"))
            .filter(|profile| !profile.is_empty())
            .unwrap_or("default")
            .trim_matches('.')
            .to_string(),
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
        }
    }
}

impl Error for EnvImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Metadata(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<MetadataError> for EnvImportError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<serde_json::Error> for EnvImportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
