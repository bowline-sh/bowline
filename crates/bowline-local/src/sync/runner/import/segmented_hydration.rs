use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read},
};

use bowline_core::{ids::WorkspaceId, workspace_graph::SegmentLocator};
use bowline_storage::{
    ByteStore, CacheError, ContentVerification, LocalContentCache, ObjectKey,
    RangeHydrationRequest, StorageKey,
};

use super::content_locator_for_segment;

pub(super) struct SegmentedHydrationRequest<'a> {
    pub(super) segments: &'a [SegmentLocator],
    pub(super) pack_epochs: &'a BTreeMap<String, u32>,
    pub(super) cache: &'a LocalContentCache,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) workspace_id: &'a WorkspaceId,
    pub(super) content_key: [u8; 32],
    pub(super) storage_key: StorageKey,
}

pub(super) struct SegmentedHydrationReader<'a> {
    request: SegmentedHydrationRequest<'a>,
    next_segment: usize,
    current: Cursor<Vec<u8>>,
    cache_failure: Option<CacheError>,
}

impl<'a> SegmentedHydrationReader<'a> {
    pub(super) fn new(request: SegmentedHydrationRequest<'a>) -> Self {
        Self {
            request,
            next_segment: 0,
            current: Cursor::new(Vec::new()),
            cache_failure: None,
        }
    }

    pub(super) fn take_cache_failure(&mut self) -> Option<CacheError> {
        self.cache_failure.take()
    }

    fn load_next_segment(&mut self) -> io::Result<bool> {
        let Some(segment) = self.request.segments.get(self.next_segment) else {
            return Ok(false);
        };
        let key_epoch = self
            .request
            .pack_epochs
            .get(segment.pack_id.as_str())
            .copied()
            .ok_or_else(|| io::Error::other("segment pack object is missing"))?;
        let object_key = match ObjectKey::from_pack_id(&segment.pack_id) {
            Ok(object_key) => object_key,
            Err(error) => return self.fail_with_cache_error(error.into()),
        };
        let locator = content_locator_for_segment(segment);
        let bytes = match self.request.cache.hydrate_record_from_range(
            self.request.byte_store,
            RangeHydrationRequest {
                object_key: &object_key,
                workspace_id: self.request.workspace_id,
                locator: &locator,
                content_key: self.request.content_key,
                content_verification: ContentVerification::AuthenticatedSegment,
                key: self.request.storage_key,
                key_epoch,
            },
        ) {
            Ok(bytes) => bytes,
            Err(error) => return self.fail_with_cache_error(error),
        };
        if bytes.len() as u64 != segment.plaintext_length {
            return Err(io::Error::other(
                "segment plaintext length did not match layout",
            ));
        }
        self.current = Cursor::new(bytes);
        self.next_segment += 1;
        Ok(true)
    }

    fn fail_with_cache_error(&mut self, error: CacheError) -> io::Result<bool> {
        self.cache_failure = Some(error);
        Err(io::Error::other("segmented content hydration failed"))
    }
}

impl Read for SegmentedHydrationReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.current.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }
            if !self.load_next_segment()? {
                return Ok(0);
            }
        }
    }
}
