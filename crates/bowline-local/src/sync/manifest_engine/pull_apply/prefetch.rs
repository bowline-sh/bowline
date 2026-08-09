//! Complete bounded blob prefetch for one deterministic apply plan.

use std::collections::BTreeMap;
use std::io::Write;

use super::{FsOp, FsOpKind};
use crate::sync::manifest_engine::manifest::{BlobKey, ManifestEntry, ManifestKey};
use crate::sync::manifest_engine::remote::{
    BlobPrefetchRequest, BlobReaderUpload, BlobUpload, ManifestBatchUpload, ManifestUpload,
    PrefetchedBlobs, RemoteObjects, TransportError,
};

const MAX_PREFETCHED_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const SEALED_BLOB_OVERHEAD_ALLOWANCE: u64 = 4 * 1024;

// An apply materializes nothing until its prefetch returns, so fetching a whole
// plan up front makes the first file wait for the last blob. A freshly saved
// file landed behind a build's worth of churn — 257 blobs, each carrying fixed
// request, decrypt and fsync cost — and missed its propagation budget while its
// own bytes sat ready. Windows bound that wait to one window's fetches.
//
// Both bounds are load-bearing: many tiny files and few large ones produce the
// same delay by different routes, so neither a count nor a byte cap alone is
// enough.
const APPLY_WINDOW_MAX_FILES: usize = 16;
const APPLY_WINDOW_MAX_PREFETCH_BYTES: u64 = 8 * 1024 * 1024;

/// Split an already-ordered plan into windows that are prefetched and applied in
/// turn. Ordering is the caller's: windows preserve it exactly, so the delete,
/// parent and Git-rank constraints the sort established still hold.
pub(super) fn apply_windows(ops: &[FsOp]) -> Vec<&[FsOp]> {
    let mut windows = Vec::new();
    let mut start = 0;
    let mut files = 0usize;
    let mut bytes = 0u64;
    for (index, op) in ops.iter().enumerate() {
        let op_bytes = prefetch_request(op).map_or(0, |(_, bytes)| bytes);
        let full = files >= APPLY_WINDOW_MAX_FILES;
        let overflows =
            files > 0 && bytes.saturating_add(op_bytes) > APPLY_WINDOW_MAX_PREFETCH_BYTES;
        if full || overflows {
            windows.push(&ops[start..index]);
            start = index;
            files = 0;
            bytes = 0;
        }
        files += 1;
        bytes = bytes.saturating_add(op_bytes);
    }
    if start < ops.len() {
        windows.push(&ops[start..]);
    }
    windows
}

/// The blob this op fetches and what it costs, or `None` when it fetches nothing
/// (a delete, a mode change, or a blob too large to prefetch at all). One place
/// decides prefetchability so a window's accounting cannot drift from the
/// request set it is meant to bound.
fn prefetch_request(op: &FsOp) -> Option<(&BlobKey, u64)> {
    let entry = match &op.kind {
        FsOpKind::Install(entry) | FsOpKind::ConflictAside(entry) => entry,
        FsOpKind::Delete | FsOpKind::ModeChange(_) => return None,
    };
    let ManifestEntry::File { size, blob_key, .. } = entry else {
        return None;
    };
    let transfer_budget = size.saturating_add(SEALED_BLOB_OVERHEAD_ALLOWANCE);
    (transfer_budget <= MAX_PREFETCHED_BLOB_BYTES).then_some((blob_key, transfer_budget))
}

pub(super) fn prefetch_objects<'a, O: RemoteObjects>(
    ops: &[FsOp],
    inner: &'a O,
) -> Result<PrefetchedRemoteObjects<'a, O>, TransportError> {
    let requests = blob_prefetch_requests(ops);
    let blobs = inner.prefetch_blobs(&requests)?;
    Ok(PrefetchedRemoteObjects { inner, blobs })
}

fn blob_prefetch_requests(ops: &[FsOp]) -> Vec<BlobPrefetchRequest> {
    let mut requests = BTreeMap::<BlobKey, u64>::new();
    for op in ops {
        let Some((blob_key, transfer_budget)) = prefetch_request(op) else {
            continue;
        };
        requests
            .entry(blob_key.clone())
            .and_modify(|byte_len| *byte_len = (*byte_len).max(transfer_budget))
            .or_insert(transfer_budget);
    }
    requests
        .into_iter()
        .map(|(key, byte_len)| BlobPrefetchRequest { key, byte_len })
        .collect()
}

pub(super) struct PrefetchedRemoteObjects<'a, O> {
    inner: &'a O,
    blobs: PrefetchedBlobs,
}

impl<O: RemoteObjects> RemoteObjects for PrefetchedRemoteObjects<'_, O> {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.inner.put_blob(upload)
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        self.inner.put_blob_reader(upload)
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.inner.put_manifest(upload)
    }

    fn put_manifests(&self, uploads: &[ManifestBatchUpload]) -> Result<(), TransportError> {
        self.inner.put_manifests(uploads)
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        match self.blobs.get(key) {
            Some(blob) => Ok(blob.clone()),
            None => self.inner.get_blob(key),
        }
    }

    fn get_blob_to_writer(
        &self,
        key: &BlobKey,
        writer: &mut dyn Write,
    ) -> Result<u64, TransportError> {
        let Some(blob) = self.blobs.get(key) else {
            return self.inner.get_blob_to_writer(key, writer);
        };
        writer
            .write_all(blob)
            .map_err(|error| TransportError::new("write-prefetched-blob", error.to_string()))?;
        Ok(blob.len() as u64)
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.inner.get_manifest(key)
    }
}

#[cfg(test)]
mod tests {
    use crate::sync::manifest_engine::engine_test_support::TestEngine;

    #[test]
    fn pull_prefetches_the_complete_distinct_blob_set_before_apply() {
        let mut engine = TestEngine::new("pull-complete-prefetch");
        let first = engine.remote_file(b"first remote bytes");
        let second = engine.remote_file(b"second remote bytes");
        engine.publish(&[
            ("a.txt", first.clone()),
            ("b.txt", second.clone()),
            ("copy.txt", first),
        ]);

        engine.pull();

        let calls = engine.remote.prefetch_requests();
        assert_eq!(
            calls.len(),
            1,
            "a plan that fits one window is fetched in one call"
        );
        assert_eq!(calls[0].len(), 2, "duplicate content is fetched once");
        assert!(
            calls[0].windows(2).all(|pair| pair[0].key < pair[1].key),
            "prefetch order is deterministic by physical blob key"
        );
        assert_eq!(engine.read("a.txt"), b"first remote bytes");
        assert_eq!(engine.read("b.txt"), b"second remote bytes");
        assert_eq!(engine.read("copy.txt"), b"first remote bytes");
    }

    // The defect this bounds: a pull installs nothing until its prefetch returns,
    // so one call for the whole plan makes every file wait for the last blob. A
    // file saved after a noisy build waited behind hundreds of unrelated blobs and
    // missed its propagation budget. What matters is not that several calls happen
    // but that the FIRST one is bounded, because that is what the first install
    // waits on.
    #[test]
    fn a_plan_larger_than_a_window_is_prefetched_in_bounded_windows() {
        let mut engine = TestEngine::new("pull-windowed-prefetch");
        let mut entries = Vec::new();
        for index in 0..40u32 {
            let contents = format!("remote bytes {index}");
            let blob = engine.remote_file(contents.as_bytes());
            entries.push((format!("file-{index:02}.txt"), contents, blob));
        }
        let published: Vec<(&str, _)> = entries
            .iter()
            .map(|(path, _, blob)| (path.as_str(), blob.clone()))
            .collect();
        engine.publish(&published);

        engine.pull();

        let calls = engine.remote.prefetch_requests();
        assert!(
            calls.len() > 1,
            "40 distinct blobs were fetched in a single call: {} call(s)",
            calls.len()
        );
        assert!(
            calls[0].len() <= super::APPLY_WINDOW_MAX_FILES,
            "the first install waits on {} blobs, more than one window",
            calls[0].len()
        );
        for (path, contents, _) in &entries {
            assert_eq!(
                engine.read(path),
                contents.as_bytes(),
                "windowing must not lose or reorder content for {path}"
            );
        }
    }
}
