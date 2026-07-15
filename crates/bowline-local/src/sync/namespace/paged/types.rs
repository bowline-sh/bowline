use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::Arc,
};

use bowline_core::{
    ids::{ContentLayoutId, NamespacePageId, SegmentPageId},
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
};

use super::{
    codec::NamespacePage,
    layout::{ContentLayoutRecord, SegmentPage},
};

pub use super::metadata::{
    CONTENT_LAYOUT_FORMAT_VERSION, INLINE_SEGMENT_MAX_BYTES, MAX_NAMESPACE_DEPTH,
    MAX_SEGMENTS_PER_LAYOUT, MetadataIdentityKey, MetadataPlaintextRecord, MetadataRecordKind,
    MetadataRecordSummary, NAMESPACE_PAGE_FORMAT_VERSION, NAMESPACE_PAGE_MAX_BYTES,
    NAMESPACE_PAGE_MIN_BYTES, NAMESPACE_PAGE_TARGET_BYTES, SEGMENT_PAGE_FORMAT_VERSION,
    SEGMENT_PAGE_MAX_BYTES, SEGMENT_PAGE_TARGET_BYTES,
};

struct LoadedMetadataRecord {
    summary: MetadataRecordSummary,
    plaintext: Arc<[u8]>,
}

pub(crate) trait PagedRecordSource {
    fn metadata_identity_key(&self) -> MetadataIdentityKey;

    fn load_record(
        &self,
        kind: MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError>;

    fn prefetch_records(
        &self,
        _kind: MetadataRecordKind,
        _logical_ids: &[String],
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError> {
        context.ensure_active()
    }
}

#[derive(Clone)]
struct PageStoreLayer {
    identity_key: MetadataIdentityKey,
    namespace_pages: BTreeMap<NamespacePageId, Arc<[u8]>>,
    content_layouts: BTreeMap<ContentLayoutId, Arc<[u8]>>,
    segment_pages: BTreeMap<SegmentPageId, Arc<[u8]>>,
    base: Option<PageStore>,
    source: Option<Arc<dyn PagedRecordSource + Send + Sync>>,
}

impl PageStoreLayer {
    fn empty(identity_key: MetadataIdentityKey) -> Self {
        Self {
            identity_key,
            namespace_pages: BTreeMap::new(),
            content_layouts: BTreeMap::new(),
            segment_pages: BTreeMap::new(),
            base: None,
            source: None,
        }
    }
}

#[derive(Clone)]
pub struct PageStore {
    layer: Arc<PageStoreLayer>,
}

impl fmt::Debug for PageStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PageStore")
            .field("namespace_pages", &self.namespace_page_count())
            .field("content_layouts", &self.content_layout_count())
            .field("segment_pages", &self.segment_page_count())
            .field("has_source", &self.has_source())
            .finish()
    }
}

impl PageStore {
    pub fn with_identity_key(identity_key: MetadataIdentityKey) -> Self {
        Self {
            layer: Arc::new(PageStoreLayer::empty(identity_key)),
        }
    }

    pub(crate) fn from_source(source: Arc<dyn PagedRecordSource + Send + Sync>) -> Self {
        let identity_key = source.metadata_identity_key();
        Self {
            layer: Arc::new(PageStoreLayer {
                source: Some(source),
                ..PageStoreLayer::empty(identity_key)
            }),
        }
    }

    pub(crate) fn overlay(base: Self) -> Self {
        let identity_key = base.identity_key();
        Self {
            layer: Arc::new(PageStoreLayer {
                base: Some(base),
                ..PageStoreLayer::empty(identity_key)
            }),
        }
    }

    pub fn identity_key(&self) -> MetadataIdentityKey {
        self.layer.identity_key
    }

    pub fn insert_verified(
        &mut self,
        kind: MetadataRecordKind,
        logical_id: &str,
        plaintext: Vec<u8>,
    ) -> Result<(), NamespaceReadError> {
        let prefix = match kind {
            MetadataRecordKind::NamespacePage => "nsp",
            MetadataRecordKind::ContentLayout => "ctl",
            MetadataRecordKind::SegmentPage => "sgp",
        };
        if super::codec::logical_id(prefix, self.identity_key(), &plaintext) != logical_id {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "metadata record logical ID mismatch",
            });
        }
        match kind {
            MetadataRecordKind::NamespacePage => {
                super::codec::decode_namespace_page(&plaintext)?;
                self.insert_namespace_page(NamespacePageId::new(logical_id), plaintext)
            }
            MetadataRecordKind::ContentLayout => {
                super::layout::decode_content_layout(&plaintext)?;
                self.insert_content_layout(ContentLayoutId::new(logical_id), plaintext)
            }
            MetadataRecordKind::SegmentPage => {
                super::layout::decode_segment_page(&plaintext)?;
                self.insert_segment_page(SegmentPageId::new(logical_id), plaintext)
            }
        }
    }

    pub fn namespace_page_count(&self) -> u64 {
        self.namespace_page_ids().len() as u64
    }

    pub fn content_layout_count(&self) -> u64 {
        self.content_layout_ids().len() as u64
    }

    pub fn segment_page_count(&self) -> u64 {
        self.segment_page_ids().len() as u64
    }

    pub fn total_encoded_bytes(&self) -> u64 {
        self.namespace_page_ids()
            .iter()
            .filter_map(|id| self.namespace_page_bytes(id))
            .chain(
                self.content_layout_ids()
                    .iter()
                    .filter_map(|id| self.content_layout_bytes(id)),
            )
            .chain(
                self.segment_page_ids()
                    .iter()
                    .filter_map(|id| self.segment_page_bytes(id)),
            )
            .map(|bytes| bytes.len() as u64)
            .sum()
    }

    pub fn namespace_page_bytes(&self, id: &NamespacePageId) -> Option<&[u8]> {
        self.layer
            .namespace_pages
            .get(id)
            .map(Arc::as_ref)
            .or_else(|| self.layer.base.as_ref()?.namespace_page_bytes(id))
    }

    pub fn content_layout_bytes(&self, id: &ContentLayoutId) -> Option<&[u8]> {
        self.layer
            .content_layouts
            .get(id)
            .map(Arc::as_ref)
            .or_else(|| self.layer.base.as_ref()?.content_layout_bytes(id))
    }

    pub fn segment_page_bytes(&self, id: &SegmentPageId) -> Option<&[u8]> {
        self.layer
            .segment_pages
            .get(id)
            .map(Arc::as_ref)
            .or_else(|| self.layer.base.as_ref()?.segment_page_bytes(id))
    }

    fn load_record(
        &self,
        kind: MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError> {
        let local = match kind {
            MetadataRecordKind::NamespacePage => self
                .layer
                .namespace_pages
                .get(&NamespacePageId::new(logical_id)),
            MetadataRecordKind::ContentLayout => self
                .layer
                .content_layouts
                .get(&ContentLayoutId::new(logical_id)),
            MetadataRecordKind::SegmentPage => self
                .layer
                .segment_pages
                .get(&SegmentPageId::new(logical_id)),
        };
        if let Some(bytes) = local {
            return Ok(Some(Arc::clone(bytes)));
        }
        if let Some(base) = &self.layer.base
            && let Some(bytes) = base.load_record(kind, logical_id, context)?
        {
            return Ok(Some(bytes));
        }
        let Some(source) = &self.layer.source else {
            return Ok(None);
        };
        source.load_record(kind, logical_id, context)
    }

    pub(crate) fn has_source(&self) -> bool {
        self.layer.source.is_some() || self.layer.base.as_ref().is_some_and(PageStore::has_source)
    }

    fn has_resident_record(&self, kind: MetadataRecordKind, logical_id: &str) -> bool {
        let local = match kind {
            MetadataRecordKind::NamespacePage => self
                .layer
                .namespace_pages
                .contains_key(&NamespacePageId::new(logical_id)),
            MetadataRecordKind::ContentLayout => self
                .layer
                .content_layouts
                .contains_key(&ContentLayoutId::new(logical_id)),
            MetadataRecordKind::SegmentPage => self
                .layer
                .segment_pages
                .contains_key(&SegmentPageId::new(logical_id)),
        };
        local
            || self
                .layer
                .base
                .as_ref()
                .is_some_and(|base| base.has_resident_record(kind, logical_id))
    }

    pub(crate) fn insert_namespace_page(
        &mut self,
        id: NamespacePageId,
        plaintext: Vec<u8>,
    ) -> Result<(), NamespaceReadError> {
        if immutable_record_matches(self.namespace_page_bytes(&id), &plaintext)? {
            return Ok(());
        }
        insert_new(
            &mut Arc::make_mut(&mut self.layer).namespace_pages,
            id,
            Arc::from(plaintext),
            "namespace page",
        )
    }

    pub(crate) fn insert_content_layout(
        &mut self,
        id: ContentLayoutId,
        plaintext: Vec<u8>,
    ) -> Result<(), NamespaceReadError> {
        if immutable_record_matches(self.content_layout_bytes(&id), &plaintext)? {
            return Ok(());
        }
        insert_new(
            &mut Arc::make_mut(&mut self.layer).content_layouts,
            id,
            Arc::from(plaintext),
            "content layout",
        )
    }

    pub(crate) fn insert_segment_page(
        &mut self,
        id: SegmentPageId,
        plaintext: Vec<u8>,
    ) -> Result<(), NamespaceReadError> {
        if immutable_record_matches(self.segment_page_bytes(&id), &plaintext)? {
            return Ok(());
        }
        insert_new(
            &mut Arc::make_mut(&mut self.layer).segment_pages,
            id,
            Arc::from(plaintext),
            "segment page",
        )
    }

    pub(crate) fn local_namespace_page_count(&self) -> u64 {
        self.layer.namespace_pages.len() as u64
    }

    pub(crate) fn local_content_layout_count(&self) -> u64 {
        self.layer.content_layouts.len() as u64
    }

    pub(crate) fn local_segment_page_count(&self) -> u64 {
        self.layer.segment_pages.len() as u64
    }

    pub(crate) fn namespace_page_ids(&self) -> BTreeSet<NamespacePageId> {
        let mut ids = self
            .layer
            .base
            .as_ref()
            .map_or_else(BTreeSet::new, PageStore::namespace_page_ids);
        ids.extend(self.layer.namespace_pages.keys().cloned());
        ids
    }

    pub(crate) fn content_layout_ids(&self) -> BTreeSet<ContentLayoutId> {
        let mut ids = self
            .layer
            .base
            .as_ref()
            .map_or_else(BTreeSet::new, PageStore::content_layout_ids);
        ids.extend(self.layer.content_layouts.keys().cloned());
        ids
    }

    pub(crate) fn segment_page_ids(&self) -> BTreeSet<SegmentPageId> {
        let mut ids = self
            .layer
            .base
            .as_ref()
            .map_or_else(BTreeSet::new, PageStore::segment_page_ids);
        ids.extend(self.layer.segment_pages.keys().cloned());
        ids
    }

    #[cfg(test)]
    pub(crate) fn insert_namespace_page_bytes(&mut self, id: NamespacePageId, bytes: Vec<u8>) {
        Arc::make_mut(&mut self.layer)
            .namespace_pages
            .insert(id, Arc::from(bytes));
    }

    #[cfg(test)]
    pub(crate) fn remove_namespace_page(&mut self, id: &NamespacePageId) {
        Arc::make_mut(&mut self.layer).namespace_pages.remove(id);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn metadata_records(&self) -> Result<Vec<MetadataRecordSummary>, NamespaceReadError> {
        let mut records = Vec::with_capacity(
            (self.namespace_page_count() + self.content_layout_count() + self.segment_page_count())
                as usize,
        );
        records.extend(
            self.namespace_page_ids()
                .into_iter()
                .map(|id| {
                    let bytes = self.namespace_page_bytes(&id).ok_or(
                        NamespaceReadError::MissingRecord {
                            record: "namespace page",
                        },
                    )?;
                    super::codec::decode_namespace_page(bytes).map(|page| MetadataRecordSummary {
                        kind: MetadataRecordKind::NamespacePage,
                        logical_id: id.as_str().to_string(),
                        encoded_bytes: bytes.len() as u64,
                        child_logical_ids: namespace_children(&page),
                        direct_pack_ids: Vec::new(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        records.extend(
            self.content_layout_ids()
                .into_iter()
                .map(|id| {
                    let bytes = self.content_layout_bytes(&id).ok_or(
                        NamespaceReadError::MissingRecord {
                            record: "content layout",
                        },
                    )?;
                    super::layout::decode_content_layout(bytes).map(|layout| {
                        MetadataRecordSummary {
                            kind: MetadataRecordKind::ContentLayout,
                            logical_id: id.as_str().to_string(),
                            encoded_bytes: bytes.len() as u64,
                            child_logical_ids: layout_children(&layout),
                            direct_pack_ids: super::layout::layout_pack_ids(&layout),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        records.extend(
            self.segment_page_ids()
                .into_iter()
                .map(|id| {
                    let bytes =
                        self.segment_page_bytes(&id)
                            .ok_or(NamespaceReadError::MissingRecord {
                                record: "segment page",
                            })?;
                    super::layout::decode_segment_page(bytes).map(|page| MetadataRecordSummary {
                        kind: MetadataRecordKind::SegmentPage,
                        logical_id: id.as_str().to_string(),
                        encoded_bytes: bytes.len() as u64,
                        child_logical_ids: segment_children(&page),
                        direct_pack_ids: super::layout::segment_page_pack_ids(&page),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(records)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn plaintext_records(&self) -> Result<Vec<MetadataPlaintextRecord>, NamespaceReadError> {
        self.metadata_records()?
            .into_iter()
            .map(|summary| {
                let plaintext = match summary.kind {
                    MetadataRecordKind::NamespacePage => {
                        self.namespace_page_bytes(&NamespacePageId::new(&summary.logical_id))
                    }
                    MetadataRecordKind::ContentLayout => {
                        self.content_layout_bytes(&ContentLayoutId::new(&summary.logical_id))
                    }
                    MetadataRecordKind::SegmentPage => {
                        self.segment_page_bytes(&SegmentPageId::new(&summary.logical_id))
                    }
                }
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "metadata plaintext",
                })?
                .to_vec();
                Ok(MetadataPlaintextRecord { summary, plaintext })
            })
            .collect()
    }

    pub fn metadata_record(
        &self,
        logical_id: &str,
    ) -> Result<Option<MetadataRecordSummary>, NamespaceReadError> {
        let summary = if logical_id.starts_with("nsp_") {
            let Some(bytes) = self.namespace_page_bytes(&NamespacePageId::new(logical_id)) else {
                return Ok(None);
            };
            let page = super::codec::decode_namespace_page(bytes)?;
            MetadataRecordSummary {
                kind: MetadataRecordKind::NamespacePage,
                logical_id: logical_id.to_string(),
                encoded_bytes: bytes.len() as u64,
                child_logical_ids: namespace_children(&page),
                direct_pack_ids: Vec::new(),
            }
        } else if logical_id.starts_with("ctl_") {
            let Some(bytes) = self.content_layout_bytes(&ContentLayoutId::new(logical_id)) else {
                return Ok(None);
            };
            let layout = super::layout::decode_content_layout(bytes)?;
            MetadataRecordSummary {
                kind: MetadataRecordKind::ContentLayout,
                logical_id: logical_id.to_string(),
                encoded_bytes: bytes.len() as u64,
                child_logical_ids: layout_children(&layout),
                direct_pack_ids: super::layout::layout_pack_ids(&layout),
            }
        } else if logical_id.starts_with("sgp_") {
            let Some(bytes) = self.segment_page_bytes(&SegmentPageId::new(logical_id)) else {
                return Ok(None);
            };
            let page = super::layout::decode_segment_page(bytes)?;
            MetadataRecordSummary {
                kind: MetadataRecordKind::SegmentPage,
                logical_id: logical_id.to_string(),
                encoded_bytes: bytes.len() as u64,
                child_logical_ids: segment_children(&page),
                direct_pack_ids: super::layout::segment_page_pack_ids(&page),
            }
        } else {
            return Ok(None);
        };
        Ok(Some(summary))
    }

    pub fn plaintext_record(
        &self,
        logical_id: &str,
    ) -> Result<Option<MetadataPlaintextRecord>, NamespaceReadError> {
        let Some(summary) = self.metadata_record(logical_id)? else {
            return Ok(None);
        };
        let plaintext = self
            .record_bytes(&summary)
            .ok_or(NamespaceReadError::MissingRecord {
                record: "metadata plaintext",
            })?
            .to_vec();
        Ok(Some(MetadataPlaintextRecord { summary, plaintext }))
    }

    pub fn record_is_new(&self, logical_id: &str) -> Result<bool, NamespaceReadError> {
        Ok(self
            .metadata_record(logical_id)?
            .is_some_and(|summary| self.is_local_record(&summary)))
    }

    #[cfg(test)]
    pub fn new_reachable_plaintext_records(
        &self,
        root: &NamespacePageId,
    ) -> Result<Vec<MetadataPlaintextRecord>, NamespaceReadError> {
        Ok(self
            .reachable_plaintext_records(root)?
            .into_iter()
            .filter(|record| self.is_local_record(&record.summary))
            .collect())
    }

    pub fn visit_new_reachable_plaintext_records<E>(
        &self,
        root: &NamespacePageId,
        context: &mut NamespaceOperationContext<'_>,
        mut visitor: impl FnMut(MetadataPlaintextRecord) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<NamespaceReadError>,
    {
        let mut pending = vec![root.as_str().to_string()];
        let mut visited = BTreeSet::new();
        while let Some(logical_id) = pending.pop() {
            context.ensure_active().map_err(E::from)?;
            if !visited.insert(logical_id.clone()) {
                continue;
            }
            let Some(record) = self.plaintext_record(&logical_id).map_err(E::from)? else {
                // An unchanged source-backed record and everything below it were
                // persisted with the base root, so the local overlay stops here.
                continue;
            };
            if !self.is_local_record(&record.summary) {
                continue;
            }
            match record.summary.kind {
                MetadataRecordKind::NamespacePage => context
                    .charge_namespace_page(record.summary.encoded_bytes)
                    .map_err(E::from)?,
                MetadataRecordKind::ContentLayout => context
                    .charge_layout_record(record.summary.encoded_bytes)
                    .map_err(E::from)?,
                MetadataRecordKind::SegmentPage => context
                    .charge_segment_page(record.summary.encoded_bytes)
                    .map_err(E::from)?,
            }
            for child in record.summary.child_logical_ids.iter().rev() {
                pending.push(child.clone());
            }
            visitor(record)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn reachable_plaintext_records(
        &self,
        root: &NamespacePageId,
    ) -> Result<Vec<MetadataPlaintextRecord>, NamespaceReadError> {
        let mut records = Vec::new();
        for logical_id in self.reachable_record_ids(root)? {
            let summary =
                self.metadata_record(&logical_id)?
                    .ok_or(NamespaceReadError::MissingRecord {
                        record: "reachable metadata record",
                    })?;
            let plaintext = self
                .record_bytes(&summary)
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "reachable metadata plaintext",
                })?
                .to_vec();
            records.push(MetadataPlaintextRecord { summary, plaintext });
        }
        Ok(records)
    }

    pub fn reachable_record_ids(
        &self,
        root: &NamespacePageId,
    ) -> Result<BTreeSet<String>, NamespaceReadError> {
        let mut reachable = BTreeSet::new();
        let mut queue = VecDeque::from([root.as_str().to_string()]);
        while let Some(id) = queue.pop_front() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            let summary = self
                .metadata_record(&id)?
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "reachable metadata record",
                })?;
            queue.extend(summary.child_logical_ids);
        }
        Ok(reachable)
    }

    pub(crate) fn reachable_record_summaries_with_context(
        &self,
        root: &NamespacePageId,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Vec<MetadataRecordSummary>, NamespaceReadError> {
        let mut reachable = BTreeSet::new();
        let mut records = Vec::new();
        let mut queue = VecDeque::from([root.as_str().to_string()]);
        while let Some(id) = queue.pop_front() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            context.ensure_active()?;
            let loaded = self.loaded_metadata_record(&id, context)?.ok_or(
                NamespaceReadError::MissingRecord {
                    record: "reachable metadata record",
                },
            )?;
            let summary = loaded.summary;
            let plaintext = loaded.plaintext;
            match summary.kind {
                MetadataRecordKind::NamespacePage => {
                    context.charge_namespace_page(plaintext.len() as u64)?;
                }
                MetadataRecordKind::ContentLayout => {
                    context.charge_layout_record(plaintext.len() as u64)?;
                }
                MetadataRecordKind::SegmentPage => {
                    context.charge_segment_page(plaintext.len() as u64)?;
                }
            }
            queue.extend(summary.child_logical_ids.iter().cloned());
            records.push(summary);
        }
        Ok(records)
    }

    fn loaded_metadata_record(
        &self,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<LoadedMetadataRecord>, NamespaceReadError> {
        let kind = if logical_id.starts_with("nsp_") {
            MetadataRecordKind::NamespacePage
        } else if logical_id.starts_with("ctl_") {
            MetadataRecordKind::ContentLayout
        } else if logical_id.starts_with("sgp_") {
            MetadataRecordKind::SegmentPage
        } else {
            return Ok(None);
        };
        let Some(bytes) = self.load_record(kind, logical_id, context)? else {
            return Ok(None);
        };
        let summary = match kind {
            MetadataRecordKind::NamespacePage => {
                let page = super::codec::decode_namespace_page(&bytes)?;
                MetadataRecordSummary {
                    kind,
                    logical_id: logical_id.to_string(),
                    encoded_bytes: bytes.len() as u64,
                    child_logical_ids: namespace_children(&page),
                    direct_pack_ids: Vec::new(),
                }
            }
            MetadataRecordKind::ContentLayout => {
                let layout = super::layout::decode_content_layout(&bytes)?;
                MetadataRecordSummary {
                    kind,
                    logical_id: logical_id.to_string(),
                    encoded_bytes: bytes.len() as u64,
                    child_logical_ids: layout_children(&layout),
                    direct_pack_ids: super::layout::layout_pack_ids(&layout),
                }
            }
            MetadataRecordKind::SegmentPage => {
                let page = super::layout::decode_segment_page(&bytes)?;
                MetadataRecordSummary {
                    kind,
                    logical_id: logical_id.to_string(),
                    encoded_bytes: bytes.len() as u64,
                    child_logical_ids: segment_children(&page),
                    direct_pack_ids: super::layout::segment_page_pack_ids(&page),
                }
            }
        };
        Ok(Some(LoadedMetadataRecord {
            summary,
            plaintext: bytes,
        }))
    }

    pub(crate) fn is_local_record(&self, summary: &MetadataRecordSummary) -> bool {
        match summary.kind {
            MetadataRecordKind::NamespacePage => self
                .layer
                .namespace_pages
                .contains_key(&NamespacePageId::new(&summary.logical_id)),
            MetadataRecordKind::ContentLayout => self
                .layer
                .content_layouts
                .contains_key(&ContentLayoutId::new(&summary.logical_id)),
            MetadataRecordKind::SegmentPage => self
                .layer
                .segment_pages
                .contains_key(&SegmentPageId::new(&summary.logical_id)),
        }
    }

    fn record_bytes(&self, summary: &MetadataRecordSummary) -> Option<&[u8]> {
        match summary.kind {
            MetadataRecordKind::NamespacePage => {
                self.namespace_page_bytes(&NamespacePageId::new(&summary.logical_id))
            }
            MetadataRecordKind::ContentLayout => {
                self.content_layout_bytes(&ContentLayoutId::new(&summary.logical_id))
            }
            MetadataRecordKind::SegmentPage => {
                self.segment_page_bytes(&SegmentPageId::new(&summary.logical_id))
            }
        }
    }
}

impl PagedRecordSource for PageStore {
    fn metadata_identity_key(&self) -> MetadataIdentityKey {
        self.identity_key()
    }

    fn load_record(
        &self,
        kind: MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError> {
        PageStore::load_record(self, kind, logical_id, context)
    }

    fn prefetch_records(
        &self,
        kind: MetadataRecordKind,
        logical_ids: &[String],
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError> {
        context.ensure_active()?;
        let mut unresolved = logical_ids
            .iter()
            .filter(|logical_id| !self.has_resident_record(kind, logical_id))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(base) = &self.layer.base {
            base.prefetch_records(kind, &unresolved, context)?;
            unresolved.retain(|logical_id| !base.has_resident_record(kind, logical_id));
        }
        if let Some(source) = &self.layer.source {
            source.prefetch_records(kind, &unresolved, context)?;
        }
        Ok(())
    }
}

fn immutable_record_matches(
    existing: Option<&[u8]>,
    plaintext: &[u8],
) -> Result<bool, NamespaceReadError> {
    let Some(existing) = existing else {
        return Ok(false);
    };
    if existing != plaintext {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "logical metadata ID collision",
        });
    }
    Ok(true)
}

fn insert_new<Id: Ord>(
    records: &mut BTreeMap<Id, Arc<[u8]>>,
    id: Id,
    plaintext: Arc<[u8]>,
    record: &'static str,
) -> Result<(), NamespaceReadError> {
    if plaintext.is_empty() {
        return Err(NamespaceReadError::MissingRecord { record });
    }
    records.insert(id, plaintext);
    Ok(())
}

impl PartialEq for PageStore {
    fn eq(&self, other: &Self) -> bool {
        self.identity_key() == other.identity_key()
            && self.namespace_page_ids() == other.namespace_page_ids()
            && self
                .namespace_page_ids()
                .iter()
                .all(|id| self.namespace_page_bytes(id) == other.namespace_page_bytes(id))
            && self.content_layout_ids() == other.content_layout_ids()
            && self
                .content_layout_ids()
                .iter()
                .all(|id| self.content_layout_bytes(id) == other.content_layout_bytes(id))
            && self.segment_page_ids() == other.segment_page_ids()
            && self
                .segment_page_ids()
                .iter()
                .all(|id| self.segment_page_bytes(id) == other.segment_page_bytes(id))
    }
}

impl Eq for PageStore {}

fn namespace_children(page: &NamespacePage) -> Vec<String> {
    let ids = match page {
        NamespacePage::Leaf { entries, .. } => entries
            .iter()
            .filter_map(|(_, value)| value.content_layout_id.as_ref())
            .map(|id| id.as_str().to_string())
            .collect(),
        NamespacePage::Branch {
            children, value, ..
        } => {
            let mut ids = children
                .iter()
                .map(|(_, id)| id.as_str().to_string())
                .collect::<Vec<_>>();
            if let Some(id) = value
                .as_ref()
                .and_then(|entry| entry.content_layout_id.as_ref())
            {
                ids.push(id.as_str().to_string());
            }
            ids
        }
    };
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn layout_children(layout: &ContentLayoutRecord) -> Vec<String> {
    match &layout.segments {
        super::layout::SegmentSequence::Inline(_) => Vec::new(),
        super::layout::SegmentSequence::Paged { root, .. } => vec![root.as_str().to_string()],
    }
}

fn segment_children(page: &SegmentPage) -> Vec<String> {
    let ids = match page {
        SegmentPage::Leaf { .. } => Vec::new(),
        SegmentPage::Index { children, .. } => children
            .iter()
            .map(|child| child.page_id.as_str().to_string())
            .collect(),
    };
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
