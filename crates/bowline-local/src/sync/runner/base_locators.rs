use std::collections::BTreeMap;

use bowline_control_plane::WorkspaceRef;
use bowline_core::{
    ids::{ContentId, SnapshotId},
    workspace_graph::ContentLayout,
};

use super::{SyncRunner, helpers::EMPTY_SNAPSHOT_ID};
use crate::sync::{SnapshotContent, content_layout_map_from_snapshot, import_snapshot_by_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BaseLocatorUnavailableReason {
    EmptyBase,
    NoLocators,
    ImportFailed,
}

impl BaseLocatorUnavailableReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyBase => "empty-base",
            Self::NoLocators => "no-locators",
            Self::ImportFailed => "import-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BaseLocatorSource {
    LocalStore,
    ImportedManifest,
    Unavailable(BaseLocatorUnavailableReason),
}

#[derive(Debug, Clone)]
pub(super) struct BaseReuseLocators {
    pub(super) locators: BTreeMap<ContentId, ContentLayout>,
    pub(super) source: BaseLocatorSource,
}

impl<'a> SyncRunner<'a> {
    pub(super) fn load_base_reuse_locators(
        &self,
        base_ref: &WorkspaceRef,
        local_head: Option<&WorkspaceRef>,
        local_head_snapshot: Option<&SnapshotContent>,
    ) -> BaseReuseLocators {
        if base_ref.snapshot_id == EMPTY_SNAPSHOT_ID {
            return BaseReuseLocators {
                locators: BTreeMap::new(),
                source: BaseLocatorSource::Unavailable(BaseLocatorUnavailableReason::EmptyBase),
            };
        }
        if local_head.is_some_and(|head| head.snapshot_id == base_ref.snapshot_id)
            && let Some(snapshot) = local_head_snapshot
            && let Some(from_manifest) =
                base_reuse_locators_from_snapshot(snapshot, BaseLocatorSource::LocalStore)
        {
            return from_manifest;
        }
        let snapshot_id = SnapshotId::new(base_ref.snapshot_id.clone());
        if let Some(from_store) = self.load_base_reuse_locators_from_store(&snapshot_id) {
            return from_store;
        }
        match import_snapshot_by_id(
            &self.options.workspace_id,
            &snapshot_id,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            crate::sync::namespace::MetadataIdentityKey::derive(
                &self.options.workspace_id,
                self.options.workspace_content_key,
            ),
        ) {
            Ok(imported) => {
                let locators =
                    content_layout_map_from_snapshot(&imported.snapshot).unwrap_or_default();
                if locators.is_empty() && imported.snapshot.manifest().entry_count > 0 {
                    BaseReuseLocators {
                        locators,
                        source: BaseLocatorSource::Unavailable(
                            BaseLocatorUnavailableReason::NoLocators,
                        ),
                    }
                } else {
                    BaseReuseLocators {
                        locators,
                        source: BaseLocatorSource::ImportedManifest,
                    }
                }
            }
            Err(_) => BaseReuseLocators {
                locators: BTreeMap::new(),
                source: BaseLocatorSource::Unavailable(BaseLocatorUnavailableReason::ImportFailed),
            },
        }
    }

    fn load_base_reuse_locators_from_store(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Option<BaseReuseLocators> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return None;
        }
        let record = self
            .with_store(|store| store.snapshot(&self.options.workspace_id, snapshot_id))
            .ok()??;
        let snapshot = self
            .with_store_sync(|store| {
                crate::sync::load_cached_snapshot(store, &record)
                    .map_err(|error| super::SyncRunnerError::StateIo(std::io::Error::other(error)))
            })
            .ok()?;
        base_reuse_locators_from_snapshot(&snapshot, BaseLocatorSource::LocalStore)
    }
}

fn base_reuse_locators_from_snapshot(
    snapshot: &SnapshotContent,
    source: BaseLocatorSource,
) -> Option<BaseReuseLocators> {
    let locators = content_layout_map_from_snapshot(snapshot).ok()?;
    if locators.is_empty() && snapshot.manifest().entry_count > 0 {
        return None;
    }
    Some(BaseReuseLocators { locators, source })
}
