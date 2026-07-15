use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{FileExecutability, NamespaceEntry, NamespaceEntryKind},
};

use super::hash_entry_part;

/// Stable identity tag for an enum variant. Tags are frozen wire bytes,
/// independent of Rust variant names, Debug output, and serde renames.
pub(crate) trait IdentityTag {
    fn identity_tag(&self) -> &'static str;
}

impl IdentityTag for NamespaceEntryKind {
    fn identity_tag(&self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
            Self::Placeholder => "placeholder",
            Self::Tombstone => "tombstone",
        }
    }
}

impl IdentityTag for PathClassification {
    fn identity_tag(&self) -> &'static str {
        match self {
            Self::WorkspaceSync => "workspace-sync",
            Self::ProjectEnv => "project-env",
            Self::Generated => "generated",
            Self::Dependency => "dependency",
            Self::Cache => "cache",
            Self::LargeFile => "large-file",
            Self::SecretLooking => "secret-looking",
            Self::LocalOnly => "local-only",
            Self::Blocked => "blocked",
        }
    }
}

impl IdentityTag for MaterializationMode {
    fn identity_tag(&self) -> &'static str {
        match self {
            Self::WorkspaceSync => "workspace-sync",
            Self::ProjectEnv => "project-env",
            Self::EncryptedSync => "encrypted-sync",
            Self::Lazy => "lazy",
            Self::StructureOnly => "structure-only",
            Self::LocalRegenerate => "local-regenerate",
            Self::LocalCache => "local-cache",
            Self::Ignore => "ignore",
            Self::LocalOnly => "local-only",
            Self::Blocked => "blocked",
        }
    }
}

impl IdentityTag for AccessFlag {
    fn identity_tag(&self) -> &'static str {
        match self {
            Self::HumanReadable => "human-readable",
            Self::AgentReadable => "agent-readable",
            Self::AgentHidden => "agent-hidden",
            Self::LeaseOnly => "lease-only",
        }
    }
}

impl IdentityTag for FileExecutability {
    fn identity_tag(&self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Executable => "executable",
        }
    }
}

fn hash_optional_part(hasher: &mut blake3::Hasher, value: Option<&[u8]>) {
    match value {
        None => hash_entry_part(hasher, &[0]),
        Some(bytes) => {
            hash_entry_part(hasher, &[1]);
            hash_entry_part(hasher, bytes);
        }
    }
}

pub(crate) fn hash_namespace_entry_identity(hasher: &mut blake3::Hasher, entry: &NamespaceEntry) {
    hash_entry_part(hasher, entry.path.as_bytes());
    hash_entry_part(hasher, entry.kind.identity_tag().as_bytes());
    hash_entry_part(hasher, entry.classification.identity_tag().as_bytes());
    hash_entry_part(hasher, entry.mode.identity_tag().as_bytes());
    hash_entry_part(hasher, &(entry.access.len() as u64).to_le_bytes());
    for flag in &entry.access {
        hash_entry_part(hasher, flag.identity_tag().as_bytes());
    }
    hash_optional_part(
        hasher,
        entry
            .content_id
            .as_ref()
            .map(|content_id| content_id.as_str().as_bytes()),
    );
    hash_optional_part(hasher, entry.symlink_target.as_deref().map(str::as_bytes));
    let byte_len = entry.byte_len.map(u64::to_le_bytes);
    hash_optional_part(hasher, byte_len.as_ref().map(|bytes| bytes.as_slice()));
    // V1 is the current active identity stream; executable metadata changes
    // workspace equality, so it is intentionally part of this domain.
    hash_entry_part(hasher, entry.executability.identity_tag().as_bytes());
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use bowline_core::{
        ids::{ContentId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind},
    };

    use super::{IdentityTag, hash_entry_part, hash_namespace_entry_identity};

    /// Test-local flat hasher: production identity moved to the plan-090
    /// chunked path; entry-tag stability is still pinned through it here.
    fn snapshot_identity_hasher(workspace_id: &WorkspaceId) -> blake3::Hasher {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"bowline snapshot identity v1\0");
        hash_entry_part(&mut hasher, workspace_id.as_str().as_bytes());
        hasher
    }
    use crate::sync::snapshot_id_from_hasher;

    const KIND_TAGS: [(NamespaceEntryKind, &str); 5] = [
        (NamespaceEntryKind::Directory, "directory"),
        (NamespaceEntryKind::File, "file"),
        (NamespaceEntryKind::Symlink, "symlink"),
        (NamespaceEntryKind::Placeholder, "placeholder"),
        (NamespaceEntryKind::Tombstone, "tombstone"),
    ];
    const CLASSIFICATION_TAGS: [(PathClassification, &str); 9] = [
        (PathClassification::WorkspaceSync, "workspace-sync"),
        (PathClassification::ProjectEnv, "project-env"),
        (PathClassification::Generated, "generated"),
        (PathClassification::Dependency, "dependency"),
        (PathClassification::Cache, "cache"),
        (PathClassification::LargeFile, "large-file"),
        (PathClassification::SecretLooking, "secret-looking"),
        (PathClassification::LocalOnly, "local-only"),
        (PathClassification::Blocked, "blocked"),
    ];
    const MODE_TAGS: [(MaterializationMode, &str); 10] = [
        (MaterializationMode::WorkspaceSync, "workspace-sync"),
        (MaterializationMode::ProjectEnv, "project-env"),
        (MaterializationMode::EncryptedSync, "encrypted-sync"),
        (MaterializationMode::Lazy, "lazy"),
        (MaterializationMode::StructureOnly, "structure-only"),
        (MaterializationMode::LocalRegenerate, "local-regenerate"),
        (MaterializationMode::LocalCache, "local-cache"),
        (MaterializationMode::Ignore, "ignore"),
        (MaterializationMode::LocalOnly, "local-only"),
        (MaterializationMode::Blocked, "blocked"),
    ];
    const ACCESS_TAGS: [(AccessFlag, &str); 4] = [
        (AccessFlag::HumanReadable, "human-readable"),
        (AccessFlag::AgentReadable, "agent-readable"),
        (AccessFlag::AgentHidden, "agent-hidden"),
        (AccessFlag::LeaseOnly, "lease-only"),
    ];
    const EXECUTABILITY_TAGS: [(FileExecutability, &str); 2] = [
        (FileExecutability::Regular, "regular"),
        (FileExecutability::Executable, "executable"),
    ];

    #[test]
    fn identity_tags_are_pinned_for_all_enums() {
        assert_pinned_tags(&KIND_TAGS, 5);
        assert_pinned_tags(&CLASSIFICATION_TAGS, 9);
        assert_pinned_tags(&MODE_TAGS, 10);
        assert_pinned_tags(&ACCESS_TAGS, 4);
        assert_pinned_tags(&EXECUTABILITY_TAGS, 2);
    }

    #[test]
    fn v1_fixture_covers_every_enum_variant() {
        let entries = v1_fixture_entries();
        let kinds = tag_set(entries.iter().map(|entry| entry.kind.identity_tag()));
        let classifications = tag_set(
            entries
                .iter()
                .map(|entry| entry.classification.identity_tag()),
        );
        let modes = tag_set(entries.iter().map(|entry| entry.mode.identity_tag()));
        let access = tag_set(
            entries
                .iter()
                .flat_map(|entry| entry.access.iter().map(IdentityTag::identity_tag)),
        );
        let executability = tag_set(
            entries
                .iter()
                .map(|entry| entry.executability.identity_tag()),
        );

        assert_eq!(kinds.len(), 5);
        assert_eq!(classifications.len(), 9);
        assert_eq!(modes.len(), 10);
        assert_eq!(access.len(), 4);
        assert_eq!(executability.len(), 2);
    }

    #[test]
    fn snapshot_identity_v1_golden_id() {
        let snapshot_id = fixture_snapshot_id(&v1_fixture_entries(), "ws_identity_fixture");

        // Golden v1 snapshot ID. If this assertion ever fails, the identity
        // encoding drifted, which re-identifies every snapshot fleet-wide.
        // Fix the encoding, never the constant except a deliberate vN bump.
        assert_eq!(snapshot_id.as_str(), "snap_713d4d66fdad21a8664f34f4");
    }

    #[test]
    fn hydration_state_never_affects_identity() {
        let golden = fixture_snapshot_id(&v1_fixture_entries(), "ws_identity_fixture");

        for state in [
            HydrationState::Local,
            HydrationState::Cold,
            HydrationState::StructureOnly,
            HydrationState::Missing,
        ] {
            let mut entries = v1_fixture_entries();
            for entry in &mut entries {
                entry.hydration_state = state;
            }
            assert_eq!(
                fixture_snapshot_id(&entries, "ws_identity_fixture").as_str(),
                golden.as_str()
            );
        }
    }

    #[test]
    fn snapshot_identity_scopes_by_workspace() {
        let entries = v1_fixture_entries();

        assert_ne!(
            fixture_snapshot_id(&entries, "ws_a"),
            fixture_snapshot_id(&entries, "ws_b")
        );
    }

    #[test]
    fn optional_field_presence_is_unambiguous() {
        let symlink_none = single_file_entry(|entry| {
            entry.symlink_target = None;
        });
        let symlink_empty = single_file_entry(|entry| {
            entry.symlink_target = Some(String::new());
        });
        assert_ne!(
            fixture_snapshot_id(&[symlink_none], "ws_optional"),
            fixture_snapshot_id(&[symlink_empty], "ws_optional")
        );

        let byte_len_none = single_file_entry(|entry| {
            entry.byte_len = None;
        });
        let byte_len_zero = single_file_entry(|entry| {
            entry.byte_len = Some(0);
        });
        assert_ne!(
            fixture_snapshot_id(&[byte_len_none], "ws_optional"),
            fixture_snapshot_id(&[byte_len_zero], "ws_optional")
        );

        let content_none = single_file_entry(|entry| {
            entry.content_id = None;
        });
        let content_empty = single_file_entry(|entry| {
            entry.content_id = Some(ContentId::new(""));
        });
        assert_ne!(
            fixture_snapshot_id(&[content_none], "ws_optional"),
            fixture_snapshot_id(&[content_empty], "ws_optional")
        );
    }

    #[test]
    fn access_flag_order_is_identity() {
        let left = single_file_entry(|entry| {
            entry.access = vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden];
        });
        let right = single_file_entry(|entry| {
            entry.access = vec![AccessFlag::AgentHidden, AccessFlag::HumanReadable];
        });

        assert_ne!(
            fixture_snapshot_id(&[left], "ws_access"),
            fixture_snapshot_id(&[right], "ws_access")
        );
    }

    fn assert_pinned_tags<T: IdentityTag>(tags: &[(T, &str)], expected_len: usize) {
        assert_eq!(tags.len(), expected_len);
        let mut unique_tags = BTreeSet::new();
        for (variant, tag) in tags {
            assert_eq!(variant.identity_tag(), *tag);
            unique_tags.insert(*tag);
        }
        assert_eq!(unique_tags.len(), tags.len());
    }

    fn tag_set<'a>(tags: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
        tags.into_iter().collect()
    }

    fn fixture_snapshot_id(
        entries: &[NamespaceEntry],
        workspace_id: &str,
    ) -> bowline_core::ids::SnapshotId {
        let workspace_id = WorkspaceId::new(workspace_id);
        let mut hasher = snapshot_identity_hasher(&workspace_id);
        for entry in entries {
            hash_namespace_entry_identity(&mut hasher, entry);
        }
        snapshot_id_from_hasher("snap", hasher)
    }

    fn v1_fixture_entries() -> Vec<NamespaceEntry> {
        (0..MODE_TAGS.len())
            .map(|index| {
                let kind = KIND_TAGS[index % KIND_TAGS.len()].0;
                let content_id = if kind == NamespaceEntryKind::File {
                    Some(ContentId::new(format!("cid_{index:02}")))
                } else {
                    None
                };
                let symlink_target = if kind == NamespaceEntryKind::Symlink {
                    Some(format!("target/{index:02}"))
                } else {
                    None
                };
                let byte_len = if kind == NamespaceEntryKind::Directory {
                    None
                } else {
                    Some(index as u64 * 7)
                };
                NamespaceEntry {
                    path: format!("p{index:02}"),
                    kind,
                    classification: CLASSIFICATION_TAGS[index % CLASSIFICATION_TAGS.len()].0,
                    mode: MODE_TAGS[index].0,
                    access: access_flags_for_index(index),
                    content_id,
                    content_layout: None,
                    symlink_target,
                    byte_len,
                    executability: EXECUTABILITY_TAGS[index % EXECUTABILITY_TAGS.len()].0,
                    hydration_state: match index % 4 {
                        0 => HydrationState::Local,
                        1 => HydrationState::Cold,
                        2 => HydrationState::StructureOnly,
                        _ => HydrationState::Missing,
                    },
                }
            })
            .collect()
    }

    fn access_flags_for_index(index: usize) -> Vec<AccessFlag> {
        let mut access = vec![ACCESS_TAGS[index % ACCESS_TAGS.len()].0];
        if index.is_multiple_of(2) {
            access.push(ACCESS_TAGS[(index + 1) % ACCESS_TAGS.len()].0);
        }
        access
    }

    fn single_file_entry(update: impl FnOnce(&mut NamespaceEntry)) -> NamespaceEntry {
        let mut entry = NamespaceEntry {
            path: "file".to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable],
            content_id: Some(ContentId::new("cid_file")),
            content_layout: None,
            symlink_target: None,
            byte_len: Some(1),
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        };
        update(&mut entry);
        entry
    }
}
