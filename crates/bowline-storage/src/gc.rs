use std::collections::BTreeSet;

use bowline_core::ids::SnapshotId;

use crate::{ByteStore, ByteStoreError, ObjectKey, RetentionState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageObjectRef {
    pub key: ObjectKey,
    pub retention_state: RetentionState,
    pub referenced_by_current_head: bool,
    pub referenced_by_snapshot: Option<SnapshotId>,
    pub referenced_by_work_view_base: bool,
    pub referenced_by_active_overlay: bool,
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageGcPlan {
    pub retained: Vec<ObjectKey>,
    pub delete_candidates: Vec<ObjectKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageGcExecutionReport {
    pub deleted: Vec<ObjectKey>,
    pub skipped: Vec<ObjectKey>,
    pub retryable_failures: Vec<StorageGcDeleteFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageGcDeleteFailure {
    pub key: ObjectKey,
    pub reason: String,
}

pub fn plan_gc(objects: &[StorageObjectRef]) -> StorageGcPlan {
    let mut retained = Vec::new();
    let mut delete_candidates = Vec::new();

    for object in objects {
        if object.referenced_by_current_head
            || object.referenced_by_snapshot.is_some()
            || object.referenced_by_work_view_base
            || object.referenced_by_active_overlay
            || !object.verified
        {
            retained.push(object.key.clone());
            continue;
        }

        match object.retention_state {
            RetentionState::DeleteEligible => delete_candidates.push(object.key.clone()),
            RetentionState::Pending
            | RetentionState::Current
            | RetentionState::OrphanCandidate
            | RetentionState::Retained => retained.push(object.key.clone()),
        }
    }

    StorageGcPlan {
        retained,
        delete_candidates,
    }
}

pub fn execute_gc_plan(
    planned: &StorageGcPlan,
    latest_objects: &[StorageObjectRef],
    store: &impl ByteStore,
) -> StorageGcExecutionReport {
    let latest_plan = plan_gc(latest_objects);
    let latest_candidates = latest_plan
        .delete_candidates
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut deleted = Vec::new();
    let mut skipped = Vec::new();
    let mut retryable_failures = Vec::new();

    for key in &planned.delete_candidates {
        if !latest_candidates.contains(key) {
            skipped.push(key.clone());
            continue;
        }

        match store.delete_object(key) {
            Ok(()) => deleted.push(key.clone()),
            Err(error) => retryable_failures.push(StorageGcDeleteFailure {
                key: key.clone(),
                reason: delete_failure_reason(error),
            }),
        }
    }

    StorageGcExecutionReport {
        deleted,
        skipped,
        retryable_failures,
    }
}

fn delete_failure_reason(error: ByteStoreError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use bowline_core::ids::SnapshotId;

    use crate::{LocalByteStore, ObjectKind};

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn gc_dry_run_keeps_current_head_and_retained_orphans() {
        let current =
            ObjectKey::new("b_0011223344556677001122334455667700112233445566770011223344556677")
                .expect("key");
        let retained_orphan =
            ObjectKey::new("b_8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff")
                .expect("key");
        let expired_orphan =
            ObjectKey::new("b_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .expect("key");
        let old =
            ObjectKey::new("b_fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
                .expect("key");

        let plan = plan_gc(&[
            StorageObjectRef {
                key: current.clone(),
                retention_state: RetentionState::DeleteEligible,
                referenced_by_current_head: true,
                referenced_by_snapshot: None,
                referenced_by_work_view_base: false,
                referenced_by_active_overlay: false,
                verified: true,
            },
            StorageObjectRef {
                key: retained_orphan.clone(),
                retention_state: RetentionState::OrphanCandidate,
                referenced_by_current_head: false,
                referenced_by_snapshot: None,
                referenced_by_work_view_base: false,
                referenced_by_active_overlay: false,
                verified: true,
            },
            StorageObjectRef {
                key: expired_orphan.clone(),
                retention_state: RetentionState::OrphanCandidate,
                referenced_by_current_head: false,
                referenced_by_snapshot: None,
                referenced_by_work_view_base: false,
                referenced_by_active_overlay: false,
                verified: true,
            },
            StorageObjectRef {
                key: old.clone(),
                retention_state: RetentionState::DeleteEligible,
                referenced_by_current_head: false,
                referenced_by_snapshot: Some(SnapshotId::new("snap_old")),
                referenced_by_work_view_base: false,
                referenced_by_active_overlay: false,
                verified: true,
            },
        ]);

        assert!(plan.delete_candidates.is_empty());
        assert_eq!(
            plan.retained,
            vec![current, retained_orphan, expired_orphan, old]
        );
    }

    #[test]
    fn gc_retains_work_view_overlay_and_unverified_objects() {
        let work_view = retained_ref(
            "b_0011223344556677001122334455667700112233445566770011223344556677",
            |object| {
                object.referenced_by_work_view_base = true;
            },
        );
        let overlay = retained_ref(
            "b_8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff",
            |object| {
                object.referenced_by_active_overlay = true;
            },
        );
        let unverified = retained_ref(
            "b_abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            |object| {
                object.verified = false;
            },
        );

        let plan = plan_gc(&[work_view.clone(), overlay.clone(), unverified.clone()]);

        assert!(plan.delete_candidates.is_empty());
        assert_eq!(
            plan.retained,
            vec![work_view.key, overlay.key, unverified.key]
        );
    }

    #[test]
    fn gc_execution_rechecks_references_before_delete() {
        let temp = TempDir::new("gc-recheck");
        let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
        let candidate_key =
            ObjectKey::new("b_0011223344556677001122334455667700112233445566770011223344556677")
                .expect("key");
        store
            .put_object(
                candidate_key.clone(),
                ObjectKind::WorkspaceFileV1,
                b"expired orphan",
                None,
            )
            .expect("put candidate");

        let planned = plan_gc(&[eligible_ref(candidate_key.clone())]);
        let mut now_referenced = eligible_ref(candidate_key.clone());
        now_referenced.referenced_by_active_overlay = true;

        let report = execute_gc_plan(&planned, &[now_referenced], &store);

        assert!(report.deleted.is_empty());
        assert_eq!(report.skipped, vec![candidate_key.clone()]);
        assert!(report.retryable_failures.is_empty());
        assert_eq!(
            store.get_object(&candidate_key).expect("object remains"),
            b"expired orphan"
        );
    }

    #[test]
    fn gc_execution_deletes_still_eligible_known_object_key() {
        let temp = TempDir::new("gc-delete");
        let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
        let candidate_key =
            ObjectKey::new("b_8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff8899aabbccddeeff")
                .expect("key");
        store
            .put_object(
                candidate_key.clone(),
                ObjectKind::WorkspaceFileV1,
                b"expired orphan",
                None,
            )
            .expect("put candidate");

        let latest = [eligible_ref(candidate_key.clone())];
        let planned = plan_gc(&latest);
        let report = execute_gc_plan(&planned, &latest, &store);

        assert_eq!(report.deleted, vec![candidate_key.clone()]);
        assert!(report.skipped.is_empty());
        assert!(report.retryable_failures.is_empty());
        assert_eq!(store.metrics().delete_count, 1);
        assert!(matches!(
            store.get_object(&candidate_key),
            Err(ByteStoreError::MissingObject { .. })
        ));
    }

    #[test]
    fn gc_execution_reports_retryable_failure_without_claiming_delete() {
        let candidate_key =
            ObjectKey::new("b_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .expect("key");
        let latest = [eligible_ref(candidate_key.clone())];
        let planned = plan_gc(&latest);
        let store = FailingDeleteStore;

        let report = execute_gc_plan(&planned, &latest, &store);

        assert!(report.deleted.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(
            report.retryable_failures,
            vec![StorageGcDeleteFailure {
                key: candidate_key,
                reason: "byte store I/O failed: delete failed".to_string(),
            }]
        );
    }

    fn eligible_ref(key: ObjectKey) -> StorageObjectRef {
        StorageObjectRef {
            key,
            retention_state: RetentionState::DeleteEligible,
            referenced_by_current_head: false,
            referenced_by_snapshot: None,
            referenced_by_work_view_base: false,
            referenced_by_active_overlay: false,
            verified: true,
        }
    }

    fn retained_ref(key: &str, edit: impl FnOnce(&mut StorageObjectRef)) -> StorageObjectRef {
        let mut object = StorageObjectRef {
            key: ObjectKey::new(key).expect("key"),
            retention_state: RetentionState::DeleteEligible,
            referenced_by_current_head: false,
            referenced_by_snapshot: None,
            referenced_by_work_view_base: false,
            referenced_by_active_overlay: false,
            verified: true,
        };
        edit(&mut object);
        object
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bowline-storage-{prefix}-{}-{sequence}",
                std::process::id()
            ));
            if path.exists() {
                std::fs::remove_dir_all(&path).expect("remove old temp dir");
            }
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct FailingDeleteStore;

    impl ByteStore for FailingDeleteStore {
        fn put_object(
            &self,
            _key: ObjectKey,
            _kind: ObjectKind,
            _bytes: &[u8],
            _created_by_device_id: Option<&bowline_core::ids::DeviceId>,
        ) -> Result<crate::ObjectMetadata, ByteStoreError> {
            Err(ByteStoreError::UnsupportedOperation("put_object"))
        }

        fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
            Err(ByteStoreError::MissingObject {
                key: key.clone(),
                component: "object",
            })
        }

        fn get_range(
            &self,
            key: &ObjectKey,
            _range: crate::ByteRange,
        ) -> Result<Vec<u8>, ByteStoreError> {
            Err(ByteStoreError::MissingObject {
                key: key.clone(),
                component: "object",
            })
        }

        fn head_object(&self, key: &ObjectKey) -> Result<crate::ObjectMetadata, ByteStoreError> {
            Err(ByteStoreError::MissingObject {
                key: key.clone(),
                component: "metadata",
            })
        }

        fn delete_object(&self, _key: &ObjectKey) -> Result<(), ByteStoreError> {
            Err(ByteStoreError::Io(std::io::Error::other("delete failed")))
        }

        fn metrics(&self) -> crate::ByteStoreMetrics {
            crate::ByteStoreMetrics::default()
        }
    }
}
