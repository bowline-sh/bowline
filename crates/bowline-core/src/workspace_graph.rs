use std::{
    io::{self, Cursor, Read},
    path::{Component, Path},
};

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    ids::{ContentId, ManifestDigest, NamespacePageId, PackId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
};

pub const SNAPSHOT_SCHEMA_VERSION: u16 = 5;

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
    pub namespace_root_id: NamespacePageId,
    pub semantic_manifest_digest: ManifestDigest,
    pub entry_count: u64,
    pub refs: Vec<WorkspaceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotDraft {
    pub schema_version: u16,
    pub snapshot_id: SnapshotId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub kind: SnapshotKind,
    pub base_snapshot_id: Option<SnapshotId>,
    pub entries: Vec<NamespaceEntry>,
    pub refs: Vec<WorkspaceRef>,
}

impl SnapshotDraft {
    pub fn from_manifest(manifest: SnapshotManifest, entries: Vec<NamespaceEntry>) -> Self {
        Self {
            schema_version: manifest.schema_version,
            snapshot_id: manifest.snapshot_id,
            workspace_id: manifest.workspace_id,
            project_id: manifest.project_id,
            kind: manifest.kind,
            base_snapshot_id: manifest.base_snapshot_id,
            entries,
            refs: manifest.refs,
        }
    }

    pub fn into_manifest(
        self,
        namespace_root_id: NamespacePageId,
        semantic_manifest_digest: ManifestDigest,
    ) -> SnapshotManifest {
        SnapshotManifest {
            schema_version: self.schema_version,
            snapshot_id: self.snapshot_id,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            kind: self.kind,
            base_snapshot_id: self.base_snapshot_id,
            namespace_root_id,
            semantic_manifest_digest,
            entry_count: self.entries.len() as u64,
            refs: self.refs,
        }
    }
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
    pub content_layout: Option<ContentLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_len: Option<u64>,
    #[serde(default)]
    pub executability: FileExecutability,
    pub hydration_state: HydrationState,
}

/// Exactly one POSIX mode bit syncs: executable. setuid/setgid/sticky and
/// group/world-write bits are deliberately normalized away because syncing
/// them would replicate privilege-escalation surface across machines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileExecutability {
    #[default]
    Regular,
    Executable,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContentLocatorWire {
    content_id: ContentId,
    storage: ContentStorage,
    raw_size: u64,
    #[serde(default)]
    pack_id: Option<PackId>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
}

impl<'de> Deserialize<'de> for ContentLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContentLocatorWire::deserialize(deserializer)?;
        let locator = Self {
            content_id: wire.content_id,
            storage: wire.storage,
            raw_size: wire.raw_size,
            pack_id: wire.pack_id,
            offset: wire.offset,
            length: wire.length,
        };
        match locator.storage {
            ContentStorage::Packed => {
                if locator.pack_id.is_some() && locator.offset.is_some() && locator.length.is_some()
                {
                    Ok(locator)
                } else {
                    Err(de::Error::custom(
                        "packed locators require packId, offset, and length",
                    ))
                }
            }
            ContentStorage::Inline => {
                if locator.pack_id.is_none() && locator.offset.is_none() && locator.length.is_none()
                {
                    Ok(locator)
                } else {
                    Err(de::Error::custom(
                        "inline locators must not carry pack ranges",
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentStorage {
    Inline,
    Packed,
}

/// A physical content representation. This is deliberately separate from
/// `NamespaceEntry::content_id`, which remains the keyed identity of the
/// complete logical file regardless of how its bytes are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ContentLayout {
    SegmentedV1 {
        logical_content_id: ContentId,
        logical_length: u64,
        segment_size: u64,
        segments: Vec<SegmentLocator>,
    },
}

impl ContentLayout {
    pub fn single_segment(locator: ContentLocator) -> Result<Self, &'static str> {
        if locator.storage != ContentStorage::Packed {
            return Err("segmented content requires packed storage");
        }
        let pack_id = locator.pack_id.ok_or("segment packId is required")?;
        let offset = locator.offset.ok_or("segment offset is required")?;
        let length = locator.length.ok_or("segment length is required")?;
        let logical_content_id = locator.content_id;
        let logical_length = locator.raw_size;
        let segment_id = SegmentId::new(logical_content_id.as_str());
        let segments = if logical_length == 0 {
            Vec::new()
        } else {
            vec![SegmentLocator {
                ordinal: 0,
                plaintext_length: logical_length,
                segment_id,
                pack_id,
                offset,
                length,
                format_version: 1,
            }]
        };
        Ok(Self::SegmentedV1 {
            logical_content_id,
            logical_length,
            segment_size: logical_length.max(1),
            segments,
        })
    }

    pub fn logical_content_id(&self) -> &ContentId {
        match self {
            Self::SegmentedV1 {
                logical_content_id, ..
            } => logical_content_id,
        }
    }

    pub fn logical_length(&self) -> u64 {
        match self {
            Self::SegmentedV1 { logical_length, .. } => *logical_length,
        }
    }

    pub fn segments(&self) -> &[SegmentLocator] {
        match self {
            Self::SegmentedV1 { segments, .. } => segments,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SegmentId(String);

impl SegmentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SegmentLocator {
    pub ordinal: u32,
    pub plaintext_length: u64,
    pub segment_id: SegmentId,
    pub pack_id: PackId,
    pub offset: u64,
    pub length: u64,
    pub format_version: u16,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ContentLayoutWire {
    SegmentedV1 {
        logical_content_id: ContentId,
        logical_length: u64,
        segment_size: u64,
        segments: Vec<SegmentLocator>,
    },
}

impl<'de> Deserialize<'de> for ContentLayout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ContentLayoutWire::deserialize(deserializer)? {
            ContentLayoutWire::SegmentedV1 {
                logical_content_id,
                logical_length,
                segment_size,
                segments,
            } => {
                if logical_content_id.as_str().is_empty() {
                    return Err(de::Error::custom(
                        "segmented-v1 logicalContentId must not be empty",
                    ));
                }
                validate_segmented_layout(logical_length, segment_size, &segments)
                    .map_err(de::Error::custom)?;
                Ok(Self::SegmentedV1 {
                    logical_content_id,
                    logical_length,
                    segment_size,
                    segments,
                })
            }
        }
    }
}

fn validate_segmented_layout(
    logical_length: u64,
    segment_size: u64,
    segments: &[SegmentLocator],
) -> Result<(), &'static str> {
    if segment_size == 0 {
        return Err("segmented-v1 segmentSize must be positive");
    }
    if logical_length == 0 {
        return if segments.is_empty() {
            Ok(())
        } else {
            Err("empty segmented-v1 content must not contain segments")
        };
    }
    if segments.is_empty() {
        return Err("non-empty segmented-v1 content requires segments");
    }

    let mut total = 0_u64;
    for (index, segment) in segments.iter().enumerate() {
        if segment.ordinal as usize != index {
            return Err("segmented-v1 ordinals must be contiguous from zero");
        }
        if segment.plaintext_length == 0 || segment.plaintext_length > segment_size {
            return Err("segmented-v1 plaintext lengths must be within segmentSize");
        }
        if index + 1 < segments.len() && segment.plaintext_length != segment_size {
            return Err("only the final segmented-v1 segment may be short");
        }
        if segment.length == 0 || segment.format_version == 0 {
            return Err("segment locator length and formatVersion must be positive");
        }
        if segment.segment_id.as_str().is_empty() || segment.pack_id.as_str().is_empty() {
            return Err("segment locator segmentId and packId must not be empty");
        }
        segment
            .offset
            .checked_add(segment.length)
            .ok_or("segment locator range overflow")?;
        total = total
            .checked_add(segment.plaintext_length)
            .ok_or("segmented-v1 logical length overflow")?;
    }
    if total != logical_length {
        return Err("segmented-v1 plaintext lengths must equal logicalLength");
    }
    Ok(())
}

pub fn workspace_content_id(workspace_content_key: [u8; 32], bytes: &[u8]) -> ContentId {
    workspace_content_id_reader(workspace_content_key, &mut Cursor::new(bytes))
        .expect("slice hashing does not fail")
}

pub fn workspace_content_id_reader(
    workspace_content_key: [u8; 32],
    reader: &mut dyn Read,
) -> io::Result<ContentId> {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(ContentId::new(format!(
        "cid_{}",
        hasher.finalize().to_hex()
    )))
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    pub fn new(path: impl AsRef<str>) -> Self {
        Self(normalize_workspace_path(path.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_equal_to_or_below(&self, root: &Self) -> bool {
        root.is_empty() || self == root || self.0.starts_with(&format!("{}/", root.0))
    }
}

impl From<String> for WorkspaceRelativePath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for WorkspaceRelativePath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl<'de> serde::Deserialize<'de> for WorkspaceRelativePath {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let path = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(path))
    }
}

pub fn is_safe_workspace_symlink_target(target: &str) -> bool {
    let normalized = normalize_workspace_path(target);
    !normalized.is_empty()
        && normalized == target
        && Path::new(target)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        ContentLayout, FileExecutability, NamespaceEntry, WorkspaceRelativePath,
        is_safe_workspace_symlink_target, normalize_workspace_path, workspace_content_id,
        workspace_content_id_reader,
    };

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
    fn content_id_reader_matches_slice_hasher() {
        let key = [11_u8; 32];
        let bytes = (0..200_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();

        assert_eq!(
            workspace_content_id(key, &bytes),
            workspace_content_id_reader(key, &mut Cursor::new(&bytes)).expect("reader hash")
        );
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

    #[test]
    fn workspace_relative_path_deserialization_preserves_canonical_invariants() {
        let path: WorkspaceRelativePath =
            serde_json::from_str(r#""./a//b/""#).expect("path deserializes");
        let canonical = WorkspaceRelativePath::new("a/b");
        let parent = WorkspaceRelativePath::new("a");
        let later = WorkspaceRelativePath::new("a/c");

        assert_eq!(path, canonical);
        assert!(path.is_equal_to_or_below(&parent));
        assert!(path < later);
        assert_eq!(
            serde_json::from_str::<WorkspaceRelativePath>(
                &serde_json::to_string(&path).expect("path serializes")
            )
            .expect("serialized path deserializes"),
            path
        );
        assert_eq!(
            serde_json::to_string(&path).expect("canonical path serializes"),
            r#""a/b""#
        );
    }

    #[test]
    fn workspace_symlink_targets_reject_every_traversal_component() {
        for target in [
            "",
            ".",
            "..",
            "../outside",
            "dir/..",
            "dir/../outside",
            "/outside",
            "./inside",
            "inside//file",
            "inside\\file",
        ] {
            assert!(
                !is_safe_workspace_symlink_target(target),
                "unsafe target accepted: {target:?}"
            );
        }
        for target in ["inside", "inside/file", "inside..name/file"] {
            assert!(
                is_safe_workspace_symlink_target(target),
                "safe target rejected: {target:?}"
            );
        }
    }

    #[test]
    fn namespace_entry_without_executability_field_defaults_to_regular() {
        let entry: NamespaceEntry = serde_json::from_str(
            r#"{
                "path": "scripts/dev.sh",
                "kind": "file",
                "classification": "workspace-sync",
                "mode": "workspace-sync",
                "access": ["human-readable"],
                "contentId": "cid_script",
                "byteLen": 12,
                "hydrationState": "local"
            }"#,
        )
        .expect("entry json");

        assert_eq!(entry.executability, FileExecutability::Regular);
    }

    #[test]
    fn content_layout_reads_segmented_files() {
        let segmented: ContentLayout = serde_json::from_str(
            r#"{
                "kind": "segmented-v1",
                "logicalContentId": "cid_whole_file",
                "logicalLength": 10,
                "segmentSize": 6,
                "segments": [
                    {
                        "ordinal": 0,
                        "plaintextLength": 6,
                        "segmentId": "seg_first",
                        "packId": "pack_segments",
                        "offset": 0,
                        "length": 22,
                        "formatVersion": 1
                    },
                    {
                        "ordinal": 1,
                        "plaintextLength": 4,
                        "segmentId": "seg_second",
                        "packId": "pack_segments",
                        "offset": 22,
                        "length": 20,
                        "formatVersion": 1
                    }
                ]
            }"#,
        )
        .expect("segmented layout");
        assert!(matches!(segmented, ContentLayout::SegmentedV1 { .. }));
        assert_eq!(segmented.logical_content_id().as_str(), "cid_whole_file");
        assert_eq!(segmented.logical_length(), 10);
    }

    #[test]
    fn segmented_layout_rejects_gaps_and_wrong_logical_length() {
        for invalid in [
            r#"{
                "kind": "segmented-v1",
                "logicalContentId": "cid_whole_file",
                "logicalLength": 6,
                "segmentSize": 6,
                "segments": [{
                    "ordinal": 1,
                    "plaintextLength": 6,
                    "segmentId": "seg_first",
                    "packId": "pack_segments",
                    "offset": 0,
                    "length": 22,
                    "formatVersion": 1
                }]
            }"#,
            r#"{
                "kind": "segmented-v1",
                "logicalContentId": "cid_whole_file",
                "logicalLength": 7,
                "segmentSize": 6,
                "segments": [{
                    "ordinal": 0,
                    "plaintextLength": 6,
                    "segmentId": "seg_first",
                    "packId": "pack_segments",
                    "offset": 0,
                    "length": 22,
                    "formatVersion": 1
                }]
            }"#,
        ] {
            serde_json::from_str::<ContentLayout>(invalid)
                .expect_err("invalid segmented layout should fail");
        }
    }
}
