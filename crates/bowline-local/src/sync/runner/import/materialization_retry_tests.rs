use super::*;
use bowline_storage::{ObjectKey, TransferOperation};

const NOW: &str = "2026-07-13T12:00:00Z";
const TASK_ID: &str = "mat_retry";

fn object_key() -> ObjectKey {
    ObjectKey::new("packs_pk_0123456789abcdef").expect("valid pack object key")
}

fn disposition(error: ByteStoreError) -> HydrationTaskFailure {
    hydration_task_failure(
        &SyncRunnerError::Cache(CacheError::Store(error)),
        TASK_ID,
        1,
        NOW,
    )
    .expect("classify hydration failure")
}

#[test]
fn retryable_http_statuses_receive_future_backoff() {
    for (status, expected_kind) in [
        (408, MaterializationFailureKind::RemoteTimeout),
        (425, MaterializationFailureKind::RemoteServiceUnavailable),
        (429, MaterializationFailureKind::RemoteRateLimited),
        (500, MaterializationFailureKind::RemoteServiceUnavailable),
        (503, MaterializationFailureKind::RemoteServiceUnavailable),
        (599, MaterializationFailureKind::RemoteServiceUnavailable),
    ] {
        let failure = disposition(ByteStoreError::HttpStatus {
            key: object_key(),
            operation: TransferOperation::Download,
            status,
        });
        assert_eq!(failure.state, MaterializationTaskState::WaitingRetry);
        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.not_before.as_deref(), Some("2026-07-13T12:00:03Z"));
    }

    let timeout = disposition(ByteStoreError::IntentFailed {
        operation: TransferOperation::Download,
        kind: IntentFailureKind::Timeout,
        detail: "timed out".to_string(),
    });
    assert_eq!(timeout.state, MaterializationTaskState::WaitingRetry);
    assert_eq!(timeout.kind, MaterializationFailureKind::RemoteTimeout);
    assert!(timeout.not_before.is_some());

    let transport = disposition(ByteStoreError::IntentFailed {
        operation: TransferOperation::Download,
        kind: IntentFailureKind::Transport,
        detail: "offline".to_string(),
    });
    assert_eq!(transport.state, MaterializationTaskState::BlockedOffline);
    assert_eq!(
        transport.kind,
        MaterializationFailureKind::TransportUnavailable
    );
    assert!(transport.not_before.is_some());

    let local_io = disposition(ByteStoreError::Io(io::Error::other("temporary local I/O")));
    assert_eq!(local_io.state, MaterializationTaskState::WaitingRetry);
    assert_eq!(local_io.kind, MaterializationFailureKind::LocalIoFailed);
    assert!(local_io.not_before.is_some());
}

#[test]
fn permanent_hydration_failures_are_not_retryable() {
    let missing = disposition(ByteStoreError::HttpStatus {
        key: object_key(),
        operation: TransferOperation::Download,
        status: 404,
    });
    assert_eq!(missing.state, MaterializationTaskState::BlockedMissing);
    assert_eq!(missing.kind, MaterializationFailureKind::ContentMissing);
    assert!(missing.not_before.is_none());

    let unauthorized = disposition(ByteStoreError::IntentFailed {
        operation: TransferOperation::Download,
        kind: IntentFailureKind::DeviceNotTrusted,
        detail: "device trust rejected".to_string(),
    });
    assert_eq!(unauthorized.state, MaterializationTaskState::Attention);
    assert_eq!(
        unauthorized.kind,
        MaterializationFailureKind::AuthorizationRequired
    );
    assert!(unauthorized.not_before.is_none());

    let unauthorized_http = disposition(ByteStoreError::HttpStatus {
        key: object_key(),
        operation: TransferOperation::Download,
        status: 401,
    });
    assert_eq!(unauthorized_http.state, MaterializationTaskState::Attention);
    assert_eq!(
        unauthorized_http.kind,
        MaterializationFailureKind::AuthorizationRequired
    );
    assert!(unauthorized_http.not_before.is_none());

    let corrupt = disposition(ByteStoreError::CorruptObject {
        key: object_key(),
        reason: "authenticated bytes did not match",
    });
    assert_eq!(corrupt.state, MaterializationTaskState::Attention);
    assert_eq!(
        corrupt.kind,
        MaterializationFailureKind::ContentIntegrityFailed
    );
    assert!(corrupt.not_before.is_none());

    let client_error = disposition(ByteStoreError::HttpStatus {
        key: object_key(),
        operation: TransferOperation::Download,
        status: 400,
    });
    assert_eq!(client_error.state, MaterializationTaskState::Attention);
    assert_eq!(
        client_error.kind,
        MaterializationFailureKind::UnsupportedHydration
    );
    assert!(client_error.not_before.is_none());

    let invalid = disposition(ByteStoreError::InvalidObjectKey {
        key: "invalid".to_string(),
        reason: "test invalid key",
    });
    assert_eq!(invalid.state, MaterializationTaskState::Attention);
    assert_eq!(
        invalid.kind,
        MaterializationFailureKind::InvalidHydrationMetadata
    );
    assert!(invalid.not_before.is_none());

    let unsupported = disposition(ByteStoreError::IntentFailed {
        operation: TransferOperation::Download,
        kind: IntentFailureKind::Other,
        detail: "unsupported".to_string(),
    });
    assert_eq!(unsupported.state, MaterializationTaskState::Attention);
    assert_eq!(
        unsupported.kind,
        MaterializationFailureKind::UnsupportedHydration
    );
    assert!(unsupported.not_before.is_none());
}

#[test]
fn retry_backoff_is_deterministic_and_bounded_by_attempt() {
    assert_eq!(
        materialization_retry_not_before(NOW, TASK_ID, 1, BOUNDED_SYNC_RETRY_POLICY)
            .expect("first retry"),
        "2026-07-13T12:00:03Z"
    );
    assert_eq!(
        materialization_retry_not_before(NOW, TASK_ID, 3, BOUNDED_SYNC_RETRY_POLICY)
            .expect("third retry"),
        "2026-07-13T12:00:09Z"
    );
    assert_eq!(
        materialization_retry_not_before(NOW, TASK_ID, u32::MAX, OFFLINE_SYNC_RETRY_POLICY,)
            .expect("bounded retry"),
        "2026-07-13T12:01:00Z"
    );
}

#[test]
fn bounded_retry_budget_exhaustion_requires_attention() {
    let failure = hydration_task_failure(
        &SyncRunnerError::Cache(CacheError::Store(ByteStoreError::HttpStatus {
            key: object_key(),
            operation: TransferOperation::Download,
            status: 503,
        })),
        TASK_ID,
        8,
        NOW,
    )
    .expect("classify exhausted hydration failure");
    assert_eq!(failure.state, MaterializationTaskState::Attention);
    assert_eq!(
        failure.kind,
        MaterializationFailureKind::RetryBudgetExhausted
    );
    assert!(failure.not_before.is_none());

    let offline = hydration_task_failure(
        &SyncRunnerError::Cache(CacheError::Store(ByteStoreError::Network {
            operation: TransferOperation::Download,
            detail: "offline".to_string(),
        })),
        TASK_ID,
        u32::MAX,
        NOW,
    )
    .expect("classify indefinite offline hydration failure");
    assert_eq!(offline.state, MaterializationTaskState::BlockedOffline);
    assert!(offline.not_before.is_some());
}
