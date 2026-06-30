use serde::{Deserialize, Serialize};

use crate::{
    ids::{ContentId, PackId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRoot {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub accepted_path: String,
    pub state: RootState,
    pub materialization_state: MaterializationState,
    pub case_sensitivity: CaseSensitivity,
    pub unicode_normalization: UnicodeNormalization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootState {
    Accepted,
    Missing,
    PendingAcceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationState {
    Ready,
    Cold,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaseSensitivity {
    Sensitive,
    Insensitive,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnicodeNormalization {
    Nfc,
    Nfd,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub root_id: String,
    pub path: String,
    pub hot_state: ProjectHotState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectHotState {
    Cold,
    Warming,
    Hot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotManifest {
    pub schema_version: u16,
    pub snapshot_id: SnapshotId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub kind: SnapshotKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<SnapshotId>,
    pub entries: Vec<NamespaceEntry>,
    pub refs: Vec<WorkspaceRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotKind {
    Base,
    Machine,
    WorkspaceHead,
    AgentOverlay,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRef {
    pub name: String,
    pub target_snapshot_id: SnapshotId,
    pub kind: RefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefKind {
    Workspace,
    Machine,
    Project,
    Lease,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceEntry {
    pub path: String,
    pub kind: NamespaceEntryKind,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessFlag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<ContentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<ContentLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_len: Option<u64>,
    pub hydration_state: HydrationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamespaceEntryKind {
    Directory,
    File,
    Symlink,
    Placeholder,
    Tombstone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HydrationState {
    Local,
    Cold,
    StructureOnly,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentLocator {
    pub content_id: ContentId,
    pub storage: ContentStorage,
    pub raw_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<PackId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunk_ids: Vec<ContentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentStorage {
    Inline,
    Packed,
    Chunked,
}

pub fn workspace_content_id(workspace_content_key: [u8; 32], bytes: &[u8]) -> ContentId {
    let hash = blake3::keyed_hash(&workspace_content_key, bytes);
    ContentId::new(format!("cid_{}", hash.to_hex()))
}

pub fn normalize_workspace_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    let normalized = normalized
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    if normalized == "." {
        String::new()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_workspace_path, workspace_content_id};

    #[test]
    fn content_id_is_workspace_scoped_and_path_independent() {
        let key_a = [7_u8; 32];
        let key_b = [9_u8; 32];

        let first = workspace_content_id(key_a, b"same file bytes");
        let same_workspace = workspace_content_id(key_a, b"same file bytes");
        let other_workspace = workspace_content_id(key_b, b"same file bytes");

        assert_eq!(first, same_workspace);
        assert_ne!(first, other_workspace);
        assert!(!first.as_str().contains("src/auth.ts"));
    }

    #[test]
    fn workspace_paths_are_canonical_relative_paths() {
        assert_eq!(normalize_workspace_path("."), "");
        assert_eq!(normalize_workspace_path("./acme//web/src/"), "acme/web/src");
        assert_eq!(
            normalize_workspace_path("/workspace/Code/acme"),
            "workspace/Code/acme"
        );
    }
}
