use std::{fs, io, thread, time::Duration};

use super::{ByteStoreError, LocalByteStore, ObjectKey, ObjectMetadata, stable_object_hash};

impl LocalByteStore {
    pub(super) fn record_put_metrics(&self, byte_len: u64) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        metrics.bytes_uploaded += byte_len;
        metrics.peak_object_bytes_in_flight = metrics.peak_object_bytes_in_flight.max(byte_len);
    }

    pub(super) fn adopt_matching_uncommitted_object(
        &self,
        metadata: &ObjectMetadata,
        expected_byte_len: u64,
        expected_hash: &str,
    ) -> Result<Option<ObjectMetadata>, ByteStoreError> {
        self.wait_for_metadata_after_object_conflict(&metadata.key)?;
        let path = self.stored_path(&metadata.key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ByteStoreError::Io(error)),
        };
        if bytes.len() as u64 != expected_byte_len || stable_object_hash(&bytes) != expected_hash {
            return Ok(None);
        }
        match self.write_metadata(metadata) {
            Ok(()) => Ok(Some(metadata.clone())),
            Err(ByteStoreError::ObjectAlreadyExists(_)) => {
                self.matching_committed_metadata(metadata).map(Some)
            }
            Err(error) => Err(error),
        }
    }

    fn wait_for_metadata_after_object_conflict(
        &self,
        key: &ObjectKey,
    ) -> Result<(), ByteStoreError> {
        for _ in 0..5 {
            match self.metadata_for_key(key) {
                Ok(_) => return Err(ByteStoreError::ObjectAlreadyExists(key.clone())),
                Err(ByteStoreError::MissingObject {
                    component: "metadata",
                    ..
                }) => thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
