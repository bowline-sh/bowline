use bowline_core::{
    ids::{ContentLayoutId, NamespacePageId},
    namespace_snapshot::{
        EntryVisitor, NamespaceDiff, NamespaceDiffVisitor, NamespaceOperationContext,
        NamespaceReadError, NamespaceScope, NamespaceSnapshotReader, NamespaceVisitControl,
        SnapshotMetadata, VisitOutcome,
    },
    workspace_graph::{NamespaceEntry, SegmentLocator, WorkspaceRelativePath},
};

use super::{
    codec::NamespaceEntryValue,
    layout::{read_layout_range, resolve_content_layout},
    snapshot::BuiltPagedNamespaceSnapshot,
    tree::{RawNamespaceDiff, diff_values, get_value, visit_prefix_values},
    types::PagedRecordSource,
};
use crate::sync::namespace::{semantic_manifest_identity_from_reader, validated_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceEntryDescriptor {
    pub entry_without_layout: NamespaceEntry,
    pub content_layout_id: Option<ContentLayoutId>,
}

pub struct PageNamespaceReader<'a> {
    metadata: &'a SnapshotMetadata,
    root: &'a NamespacePageId,
    store: &'a dyn PagedRecordSource,
}

impl<'a> PageNamespaceReader<'a> {
    pub fn new(snapshot: &'a BuiltPagedNamespaceSnapshot) -> Self {
        Self {
            metadata: &snapshot.metadata,
            root: &snapshot.namespace_root_id,
            store: &snapshot.store,
        }
    }

    pub(crate) fn from_source(
        metadata: &'a SnapshotMetadata,
        root: &'a NamespacePageId,
        source: &'a dyn PagedRecordSource,
    ) -> Self {
        Self {
            metadata,
            root,
            store: source,
        }
    }

    pub fn namespace_root_id(&self) -> &NamespacePageId {
        self.root
    }

    pub fn descriptor(
        &self,
        path: &WorkspaceRelativePath,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<NamespaceEntryDescriptor>, NamespaceReadError> {
        validated_path(path.as_str())?;
        get_value(
            self.metadata.workspace_id.as_str(),
            self.root,
            self.store,
            path.as_str().as_bytes(),
            context,
        )?
        .map(|value| descriptor_from_value(path.as_str().as_bytes().to_vec(), value))
        .transpose()
    }

    pub fn visit_prefix_descriptors(
        &self,
        prefix: &WorkspaceRelativePath,
        context: &mut NamespaceOperationContext<'_>,
        visitor: &mut dyn FnMut(
            NamespaceEntryDescriptor,
        ) -> Result<NamespaceVisitControl, NamespaceReadError>,
    ) -> Result<VisitOutcome, NamespaceReadError> {
        if !prefix.is_empty() {
            validated_path(prefix.as_str())?;
        }
        let before = context.counters().entries_visited;
        let stopped = visit_prefix_values(
            self.metadata.workspace_id.as_str(),
            self.root,
            self.store,
            prefix.as_str().as_bytes(),
            context,
            &mut |(path, value), context| {
                context.charge_entries(1)?;
                Ok(visitor(descriptor_from_value(path, value)?)? == NamespaceVisitControl::Stop)
            },
        )?;
        Ok(VisitOutcome {
            entries_visited: context.counters().entries_visited - before,
            stopped_early: stopped,
        })
    }

    pub fn content_range(
        &self,
        layout_id: &ContentLayoutId,
        logical_offset: u64,
        logical_length: u64,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Vec<SegmentLocator>, NamespaceReadError> {
        read_layout_range(
            self.metadata.workspace_id.as_str(),
            layout_id,
            self.store,
            logical_offset,
            logical_length,
            context,
        )
    }

    pub fn diff_paged(
        &self,
        other: &Self,
        scope: &NamespaceScope,
        visitor: &mut dyn NamespaceDiffVisitor,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<VisitOutcome, NamespaceReadError> {
        if self.metadata.workspace_id != other.metadata.workspace_id {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "cannot diff page graphs from different workspaces",
            });
        }
        let before = context.counters().diff_entries_visited;
        diff_values(
            self.metadata.workspace_id.as_str(),
            (self.root, self.store),
            (other.root, other.store),
            scope.prefix().as_str().as_bytes(),
            context,
            &mut |difference, context| {
                visitor.visit(match difference {
                    RawNamespaceDiff::Added(value) => {
                        NamespaceDiff::Added(other.resolve_value(value, context)?)
                    }
                    RawNamespaceDiff::Removed(value) => {
                        NamespaceDiff::Removed(self.resolve_value(value, context)?)
                    }
                    RawNamespaceDiff::Modified { before, after } => NamespaceDiff::Modified {
                        before: self.resolve_value(before, context)?,
                        after: other.resolve_value(after, context)?,
                    },
                })
            },
        )?;
        Ok(VisitOutcome {
            entries_visited: context.counters().diff_entries_visited - before,
            stopped_early: false,
        })
    }

    pub fn verify(
        &self,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError> {
        let identity = semantic_manifest_identity_from_reader(self, context)?;
        if identity.digest() != &self.metadata.semantic_manifest_digest
            || identity.snapshot_id() != &self.metadata.snapshot_id
            || identity.entries_hashed() != self.metadata.entry_count
        {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "page graph semantic identity does not match snapshot metadata",
            });
        }
        Ok(())
    }

    fn resolve_value(
        &self,
        (path, value): (Vec<u8>, NamespaceEntryValue),
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<NamespaceEntry, NamespaceReadError> {
        let descriptor = descriptor_from_value(path, value)?;
        let layout = descriptor
            .content_layout_id
            .as_ref()
            .map(|id| {
                resolve_content_layout(self.metadata.workspace_id.as_str(), id, self.store, context)
            })
            .transpose()?;
        let mut entry = descriptor.entry_without_layout;
        entry.content_layout = layout;
        Ok(entry)
    }
}

impl NamespaceSnapshotReader for PageNamespaceReader<'_> {
    fn metadata(&self) -> &SnapshotMetadata {
        self.metadata
    }

    fn get(
        &self,
        path: &WorkspaceRelativePath,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<NamespaceEntry>, NamespaceReadError> {
        validated_path(path.as_str())?;
        let value = get_value(
            self.metadata.workspace_id.as_str(),
            self.root,
            self.store,
            path.as_str().as_bytes(),
            context,
        )?;
        match value {
            Some(value) => {
                context.charge_entries(1)?;
                self.resolve_value((path.as_str().as_bytes().to_vec(), value), context)
                    .map(Some)
            }
            None => Ok(None),
        }
    }

    fn visit_prefix(
        &self,
        prefix: &WorkspaceRelativePath,
        visitor: &mut dyn EntryVisitor,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<VisitOutcome, NamespaceReadError> {
        if !prefix.is_empty() {
            validated_path(prefix.as_str())?;
        }
        let before = context.counters().entries_visited;
        let stopped = visit_prefix_values(
            self.metadata.workspace_id.as_str(),
            self.root,
            self.store,
            prefix.as_str().as_bytes(),
            context,
            &mut |value, context| {
                context.charge_entries(1)?;
                let entry = self.resolve_value(value, context)?;
                Ok(visitor.visit(&entry, context)? == NamespaceVisitControl::Stop)
            },
        )?;
        Ok(VisitOutcome {
            entries_visited: context.counters().entries_visited - before,
            stopped_early: stopped,
        })
    }

    fn diff(
        &self,
        other: &dyn NamespaceSnapshotReader,
        scope: &NamespaceScope,
        visitor: &mut dyn NamespaceDiffVisitor,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<VisitOutcome, NamespaceReadError> {
        if self.metadata.semantic_manifest_digest == other.metadata().semantic_manifest_digest {
            context.ensure_active()?;
            return Ok(VisitOutcome {
                entries_visited: 0,
                stopped_early: false,
            });
        }
        let before = context.counters().diff_entries_visited;
        struct RemovedModified<'a> {
            other: &'a dyn NamespaceSnapshotReader,
            visitor: &'a mut dyn NamespaceDiffVisitor,
        }
        impl EntryVisitor for RemovedModified<'_> {
            fn visit(
                &mut self,
                entry: &NamespaceEntry,
                context: &mut NamespaceOperationContext<'_>,
            ) -> Result<NamespaceVisitControl, NamespaceReadError> {
                context.charge_diff_entries(1)?;
                let path = validated_path(&entry.path)?;
                match self.other.get(&path, context)? {
                    None => self.visitor.visit(NamespaceDiff::Removed(entry.clone()))?,
                    Some(after) if entry != &after => {
                        self.visitor.visit(NamespaceDiff::Modified {
                            before: entry.clone(),
                            after,
                        })?
                    }
                    Some(_) => {}
                }
                Ok(NamespaceVisitControl::Continue)
            }
        }
        self.visit_prefix(
            &scope.prefix(),
            &mut RemovedModified { other, visitor },
            context,
        )?;
        struct Added<'a> {
            current: &'a PageNamespaceReader<'a>,
            visitor: &'a mut dyn NamespaceDiffVisitor,
        }
        impl EntryVisitor for Added<'_> {
            fn visit(
                &mut self,
                entry: &NamespaceEntry,
                context: &mut NamespaceOperationContext<'_>,
            ) -> Result<NamespaceVisitControl, NamespaceReadError> {
                context.charge_diff_entries(1)?;
                if self
                    .current
                    .get(&WorkspaceRelativePath::new(&entry.path), context)?
                    .is_none()
                {
                    self.visitor.visit(NamespaceDiff::Added(entry.clone()))?;
                }
                Ok(NamespaceVisitControl::Continue)
            }
        }
        other.visit_prefix(
            &scope.prefix(),
            &mut Added {
                current: self,
                visitor,
            },
            context,
        )?;
        Ok(VisitOutcome {
            entries_visited: context.counters().diff_entries_visited - before,
            stopped_early: false,
        })
    }
}

fn descriptor_from_value(
    path: Vec<u8>,
    value: NamespaceEntryValue,
) -> Result<NamespaceEntryDescriptor, NamespaceReadError> {
    let path = String::from_utf8(path).map_err(|_| NamespaceReadError::CorruptGraph {
        reason: "namespace path is not UTF-8",
    })?;
    validated_path(&path)?;
    let content_layout_id = value.content_layout_id.clone();
    Ok(NamespaceEntryDescriptor {
        entry_without_layout: value.into_entry(path, None),
        content_layout_id,
    })
}
