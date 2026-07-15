use std::{
    collections::BTreeSet,
    io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration as StdDuration,
};

use bowline_control_plane::{ObjectPointer, WorkspaceRef};
use bowline_core::retry::{
    BOUNDED_SYNC_RETRY_POLICY, OFFLINE_SYNC_RETRY_POLICY, RetryBackoffPolicy,
};
use bowline_core::workspace_graph::NamespaceEntryKind;
use bowline_storage::{ByteStoreError, CacheError, IntentFailureKind};
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::super::materialization_guard::{
    MaterializationBoundary, MaterializationRequest, materialize_snapshot_guarded,
};
use super::super::{LongOperationCancellationPoint, SyncRunner, SyncRunnerError};
use super::materialization_planning::materialization_task_matches_target;
use crate::metadata::{
    MATERIALIZATION_TASK_HEARTBEAT_SECONDS, MaterializationFailureKind, MaterializationTaskFence,
    MaterializationTaskFinish, MaterializationTaskId, MaterializationTaskRecord,
    MaterializationTaskState, MetadataError, MetadataStore,
};
use crate::sync::{SnapshotContent, unresolved_conflict_paths};

struct MaterializationLeaseSupervisor {
    stop: Arc<(Mutex<bool>, Condvar)>,
    lost: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MaterializationLeaseSupervisor {
    fn start(
        database_path: PathBuf,
        task_id: MaterializationTaskId,
        claim_token: String,
        claim_generation: u64,
    ) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let lost = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_lost = Arc::clone(&lost);
        let handle = thread::spawn(move || {
            let store = match MetadataStore::open(database_path) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("bowline-sync materialization lease store open failed: {error}");
                    thread_lost.store(true, Ordering::Release);
                    return;
                }
            };
            loop {
                let (lock, wake) = &*thread_stop;
                let stopped = match lock.lock() {
                    Ok(stopped) => stopped,
                    Err(_) => {
                        eprintln!("bowline-sync materialization lease stop lock was poisoned");
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                };
                if *stopped {
                    return;
                }
                let wait = wake.wait_timeout(
                    stopped,
                    StdDuration::from_secs(MATERIALIZATION_TASK_HEARTBEAT_SECONDS),
                );
                let Ok((stopped, timeout)) = wait else {
                    eprintln!("bowline-sync materialization lease wait lock was poisoned");
                    thread_lost.store(true, Ordering::Release);
                    return;
                };
                if *stopped {
                    return;
                }
                if !timeout.timed_out() {
                    continue;
                }
                drop(stopped);
                let now = match materialization_clock_now() {
                    Ok(now) => now,
                    Err(error) => {
                        eprintln!("bowline-sync materialization lease clock failed: {error}");
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                };
                match store.renew_materialization_task_claim(
                    &task_id,
                    &claim_token,
                    claim_generation,
                    &now,
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                    Err(error) => {
                        eprintln!("bowline-sync materialization lease renewal failed: {error}");
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                }
            }
        });
        Self {
            stop,
            lost,
            handle: Some(handle),
        }
    }

    fn ensure_current(&self, path: &str) -> Result<(), SyncRunnerError> {
        if self.lost.load(Ordering::Acquire) {
            Err(SyncRunnerError::MaterializationTaskFenceLost(
                path.to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for MaterializationLeaseSupervisor {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
            wake.notify_one();
        } else {
            eprintln!("bowline-sync materialization lease stop lock was poisoned");
        }
        if self
            .handle
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            eprintln!("bowline-sync materialization lease supervisor panicked");
        }
    }
}

impl SyncRunner<'_> {
    pub(super) fn execute_imported_materialization_tasks(
        &self,
        remote_ref: &WorkspaceRef,
        base: Option<&SnapshotContent>,
        target: SnapshotContent,
        pack_pointers: &[ObjectPointer],
    ) -> Result<SnapshotContent, SyncRunnerError> {
        loop {
            self.check_reconciling_cancellation(
                LongOperationCancellationPoint::BetweenMaterializationTasks,
            )?;
            let claim_token = materialization_claim_token()?;
            let claim_now = materialization_clock_now()?;
            let claimed = self.with_store(|store| {
                store.claim_next_materialization_task(
                    &self.options.workspace_id,
                    self.options.device_id.as_str(),
                    &claim_token,
                    &claim_now,
                )
            })?;
            let Some(task) = claimed else {
                break;
            };
            self.execute_imported_materialization_task(
                remote_ref,
                base,
                &target,
                pack_pointers,
                &task,
            )?;
        }
        let retry_pending = self.with_store(|store| {
            store.has_pending_materialization_retry(
                &self.options.workspace_id,
                &target.manifest().snapshot_id,
            )
        })?;
        if retry_pending {
            return Err(SyncRunnerError::MaterializationRetryPending);
        }
        Ok(target)
    }

    fn execute_imported_materialization_task(
        &self,
        remote_ref: &WorkspaceRef,
        base: Option<&SnapshotContent>,
        target: &SnapshotContent,
        pack_pointers: &[ObjectPointer],
        task: &MaterializationTaskRecord,
    ) -> Result<(), SyncRunnerError> {
        if task.snapshot_id != target.manifest().snapshot_id {
            return Err(SyncRunnerError::SupersededMaterializationSnapshot(
                task.snapshot_id.as_str().to_string(),
            ));
        }
        if !materialization_task_matches_target(task, target)? {
            return Err(MetadataError::InvalidStorageMetadata(
                "claimed materialization task does not match its immutable target manifest"
                    .to_string(),
            )
            .into());
        }
        let active_token = task.claim_token.as_deref().ok_or_else(|| {
            MetadataError::InvalidStorageMetadata(
                "claimed materialization task is missing its claim token".to_string(),
            )
        })?;
        self.renew_materialization_task_claim(task, active_token)?;
        let lease = MaterializationLeaseSupervisor::start(
            self.metadata_db_path(),
            task.id.clone(),
            active_token.to_string(),
            task.claim_generation,
        );
        if !self.materialization_task_fence_is_current(task, active_token)? {
            if !self.finish_materialization_task(
                task,
                active_token,
                MaterializationTaskState::BlockedConflict,
                Some(MaterializationFailureKind::PathFenceNotCurrent),
                Some("local work or an unresolved conflict owns this path"),
                None,
            )? {
                return Err(SyncRunnerError::MaterializationTaskFenceLost(
                    task.path.clone(),
                ));
            }
            return Ok(());
        }

        let task_target = if task.expected_kind == NamespaceEntryKind::File {
            match self.hydrate_imported_materialization_task(
                target.clone(),
                pack_pointers,
                &task.path,
            ) {
                Ok(target) => target,
                Err(error) => {
                    let failure = hydration_task_failure(
                        &error,
                        task.id.as_str(),
                        task.attempt_count,
                        task.claimed_at.as_deref().ok_or_else(|| {
                            MetadataError::InvalidStorageMetadata(
                                "claimed materialization task is missing its claim time"
                                    .to_string(),
                            )
                        })?,
                    )?;
                    if !self.finish_materialization_task(
                        task,
                        active_token,
                        failure.state,
                        Some(failure.kind),
                        Some(failure.summary),
                        failure.not_before.as_deref(),
                    )? {
                        return Err(SyncRunnerError::MaterializationTaskFenceLost(
                            task.path.clone(),
                        ));
                    }
                    report_materialization_task_failure(&task.id, failure.kind);
                    if failure.not_before.is_some() {
                        return Err(error);
                    }
                    return Ok(());
                }
            }
        } else {
            target.clone()
        };

        let intentionally_absent_paths = if task.expected_kind == NamespaceEntryKind::Tombstone {
            BTreeSet::from([task.path.clone()])
        } else {
            BTreeSet::new()
        };
        let materialized = materialize_snapshot_guarded(
            MaterializationRequest::task(
                &self.options.state_root,
                &self.options.root,
                base,
                &task_target,
                &intentionally_absent_paths,
                &task.path,
            ),
            |boundary| {
                if boundary != MaterializationBoundary::AfterMutation {
                    lease.ensure_current(&task.path)?;
                    self.renew_materialization_task_claim(task, active_token)?;
                    if !self.materialization_task_fence_is_current(task, active_token)? {
                        return Err(SyncRunnerError::MaterializationTaskFenceLost(
                            task.path.clone(),
                        ));
                    }
                }
                self.authorize_materialization(remote_ref, boundary)
            },
        );
        task_target
            .remove_lease_owned_files()
            .map_err(SyncRunnerError::StateIo)?;
        self.check_reconciling_cancellation(LongOperationCancellationPoint::BeforeStagePublish)?;
        self.finish_materialization_attempt(task, active_token, materialized)
    }

    fn finish_materialization_attempt(
        &self,
        task: &MaterializationTaskRecord,
        active_token: &str,
        materialized: Result<(), SyncRunnerError>,
    ) -> Result<(), SyncRunnerError> {
        match materialized {
            Ok(()) => {
                if self.finish_materialization_task(
                    task,
                    active_token,
                    MaterializationTaskState::Staged,
                    None,
                    None,
                    None,
                )? {
                    Ok(())
                } else {
                    Err(SyncRunnerError::MaterializationTaskFenceLost(
                        task.path.clone(),
                    ))
                }
            }
            Err(error @ SyncRunnerError::SyncClaimOwnershipLost)
            | Err(error @ SyncRunnerError::SyncOperationCancellationRequested)
            | Err(error @ SyncRunnerError::SupersededMaterializationSnapshot(_)) => Err(error),
            Err(SyncRunnerError::MaterializationTaskFenceLost(_)) => {
                self.finish_materialization_task(
                    task,
                    active_token,
                    MaterializationTaskState::BlockedConflict,
                    Some(MaterializationFailureKind::PathFenceNotCurrent),
                    Some("local work or an unresolved conflict owns this path"),
                    None,
                )?;
                report_materialization_task_failure(
                    &task.id,
                    MaterializationFailureKind::PathFenceNotCurrent,
                );
                Ok(())
            }
            Err(error) => {
                let not_before = materialization_retry_not_before(
                    task.claimed_at.as_deref().ok_or_else(|| {
                        MetadataError::InvalidStorageMetadata(
                            "claimed materialization task is missing its claim time".to_string(),
                        )
                    })?,
                    task.id.as_str(),
                    task.attempt_count,
                    BOUNDED_SYNC_RETRY_POLICY,
                )?;
                if !self.finish_materialization_task(
                    task,
                    active_token,
                    MaterializationTaskState::WaitingRetry,
                    Some(MaterializationFailureKind::WorkspaceMutationFailed),
                    Some("the workspace path could not be updated safely"),
                    Some(&not_before),
                )? {
                    return Err(SyncRunnerError::MaterializationTaskFenceLost(
                        task.path.clone(),
                    ));
                }
                report_materialization_task_failure(
                    &task.id,
                    MaterializationFailureKind::WorkspaceMutationFailed,
                );
                Err(error)
            }
        }
    }

    fn materialization_task_fence_is_current(
        &self,
        task: &MaterializationTaskRecord,
        claim_token: &str,
    ) -> Result<bool, SyncRunnerError> {
        let now = materialization_clock_now()?;
        let conflict_paths = unresolved_conflict_paths(&self.options.state_root)?;
        self.with_store(|store| {
            store.materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &task.id,
                claim_token,
                claim_generation: task.claim_generation,
                snapshot_id: &task.snapshot_id,
                path: &task.path,
                expected_kind: task.expected_kind,
                expected_content_id: task.expected_content_id.as_ref(),
                unresolved_conflict_paths: &conflict_paths,
                now: &now,
            })
        })
    }

    fn renew_materialization_task_claim(
        &self,
        task: &MaterializationTaskRecord,
        claim_token: &str,
    ) -> Result<(), SyncRunnerError> {
        let now = materialization_clock_now()?;
        let renewed = self.with_store(|store| {
            store.renew_materialization_task_claim(
                &task.id,
                claim_token,
                task.claim_generation,
                &now,
            )
        })?;
        if renewed {
            Ok(())
        } else {
            Err(SyncRunnerError::MaterializationTaskFenceLost(
                task.path.clone(),
            ))
        }
    }

    fn finish_materialization_task(
        &self,
        task: &MaterializationTaskRecord,
        claim_token: &str,
        state: MaterializationTaskState,
        error_kind: Option<MaterializationFailureKind>,
        error: Option<&str>,
        not_before: Option<&str>,
    ) -> Result<bool, SyncRunnerError> {
        let now = materialization_clock_now()?;
        self.with_store(|store| {
            store.finish_materialization_task(&MaterializationTaskFinish {
                id: &task.id,
                claim_token,
                claim_generation: task.claim_generation,
                state,
                error_kind,
                error,
                not_before,
                now: &now,
            })
        })
    }
}

fn materialization_claim_token() -> Result<String, SyncRunnerError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        SyncRunnerError::StateIo(io::Error::other(format!(
            "materialization claim token generation failed: {error}"
        )))
    })?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

fn materialization_clock_now() -> Result<String, SyncRunnerError> {
    OffsetDateTime::now_utc().format(&Rfc3339).map_err(|_| {
        MetadataError::InvalidStorageMetadata(
            "materialization clock could not be formatted".to_string(),
        )
        .into()
    })
}

fn report_materialization_task_failure(
    task_id: &MaterializationTaskId,
    kind: MaterializationFailureKind,
) {
    eprintln!(
        "bowline-sync materialization task {} did not complete: {}",
        task_id.as_str(),
        kind.as_str(),
    );
}

#[derive(Debug, PartialEq, Eq)]
struct HydrationTaskFailure {
    state: MaterializationTaskState,
    kind: MaterializationFailureKind,
    summary: &'static str,
    not_before: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HydrationRetryPolicy {
    None,
    Bounded,
    Offline,
}

struct HydrationFailureClass {
    state: MaterializationTaskState,
    kind: MaterializationFailureKind,
    summary: &'static str,
    retry_policy: HydrationRetryPolicy,
}

fn hydration_task_failure(
    error: &SyncRunnerError,
    task_id: &str,
    attempt_count: u32,
    now: &str,
) -> Result<HydrationTaskFailure, SyncRunnerError> {
    let mut class = classify_hydration_failure(error);
    if class.retry_policy == HydrationRetryPolicy::Bounded
        && BOUNDED_SYNC_RETRY_POLICY.is_exhausted(attempt_count)
    {
        class.state = MaterializationTaskState::Attention;
        class.kind = MaterializationFailureKind::RetryBudgetExhausted;
        class.summary = "encrypted content could not be downloaded after repeated attempts";
    }
    let backoff_policy = match class.retry_policy {
        HydrationRetryPolicy::Bounded if class.state == MaterializationTaskState::WaitingRetry => {
            Some(BOUNDED_SYNC_RETRY_POLICY)
        }
        HydrationRetryPolicy::Offline => Some(OFFLINE_SYNC_RETRY_POLICY),
        HydrationRetryPolicy::None | HydrationRetryPolicy::Bounded => None,
    };
    Ok(HydrationTaskFailure {
        state: class.state,
        kind: class.kind,
        summary: class.summary,
        not_before: backoff_policy
            .map(|policy| materialization_retry_not_before(now, task_id, attempt_count, policy))
            .transpose()?,
    })
}

fn classify_hydration_failure(error: &SyncRunnerError) -> HydrationFailureClass {
    match error {
        SyncRunnerError::Cache(error) => classify_cache_failure(error),
        _ => attention_failure(MaterializationFailureKind::HydrationFailed),
    }
}

fn classify_cache_failure(error: &CacheError) -> HydrationFailureClass {
    match error {
        CacheError::Io(_) => local_io_failure(),
        CacheError::Store(error) => classify_byte_store_failure(error),
        CacheError::ContentIdMismatch { .. }
        | CacheError::InvalidCachedPackRange { .. }
        | CacheError::ShortCachedPackRead { .. }
        | CacheError::ShortFetchedRange { .. }
        | CacheError::MismatchedCachedPackReader { .. }
        | CacheError::Pack(_) => integrity_failure(),
        CacheError::MissingCachedBytes(_)
        | CacheError::MissingPackedLocator(_)
        | CacheError::InvalidCacheKey(_) => {
            attention_failure(MaterializationFailureKind::InvalidHydrationMetadata)
        }
    }
}

fn classify_byte_store_failure(error: &ByteStoreError) -> HydrationFailureClass {
    match error {
        ByteStoreError::Io(_) => local_io_failure(),
        ByteStoreError::Network { .. } => offline_failure(),
        ByteStoreError::HttpStatus { status, .. } => classify_http_failure(*status),
        ByteStoreError::IntentFailed { kind, .. } => classify_intent_failure(*kind),
        ByteStoreError::MissingObject { .. } => missing_failure(),
        ByteStoreError::CorruptObject { .. }
        | ByteStoreError::CorruptJournal { .. }
        | ByteStoreError::RangeOutOfBounds { .. } => integrity_failure(),
        ByteStoreError::InvalidObjectKey { .. } => {
            attention_failure(MaterializationFailureKind::InvalidHydrationMetadata)
        }
        ByteStoreError::ObjectAlreadyExists(_) | ByteStoreError::UnsupportedOperation(_) => {
            attention_failure(MaterializationFailureKind::UnsupportedHydration)
        }
    }
}

fn classify_http_failure(status: u16) -> HydrationFailureClass {
    match status {
        404 => missing_failure(),
        401 | 403 => attention_failure(MaterializationFailureKind::AuthorizationRequired),
        408 => bounded_failure(MaterializationFailureKind::RemoteTimeout),
        425 | 500..=599 => bounded_failure(MaterializationFailureKind::RemoteServiceUnavailable),
        429 => bounded_failure(MaterializationFailureKind::RemoteRateLimited),
        _ => attention_failure(MaterializationFailureKind::UnsupportedHydration),
    }
}

fn classify_intent_failure(kind: IntentFailureKind) -> HydrationFailureClass {
    match kind {
        IntentFailureKind::Timeout => bounded_failure(MaterializationFailureKind::RemoteTimeout),
        IntentFailureKind::Transport => offline_failure(),
        IntentFailureKind::DeviceNotTrusted => {
            attention_failure(MaterializationFailureKind::AuthorizationRequired)
        }
        IntentFailureKind::Other => {
            attention_failure(MaterializationFailureKind::UnsupportedHydration)
        }
    }
}

fn missing_failure() -> HydrationFailureClass {
    HydrationFailureClass {
        state: MaterializationTaskState::BlockedMissing,
        kind: MaterializationFailureKind::ContentMissing,
        summary: "required encrypted content is not locally or remotely available",
        retry_policy: HydrationRetryPolicy::None,
    }
}

fn offline_failure() -> HydrationFailureClass {
    HydrationFailureClass {
        state: MaterializationTaskState::BlockedOffline,
        kind: MaterializationFailureKind::TransportUnavailable,
        summary: "required encrypted content could not be reached",
        retry_policy: HydrationRetryPolicy::Offline,
    }
}

fn bounded_failure(kind: MaterializationFailureKind) -> HydrationFailureClass {
    HydrationFailureClass {
        state: MaterializationTaskState::WaitingRetry,
        kind,
        summary: "required encrypted content could not be downloaded yet",
        retry_policy: HydrationRetryPolicy::Bounded,
    }
}

fn local_io_failure() -> HydrationFailureClass {
    let mut failure = bounded_failure(MaterializationFailureKind::LocalIoFailed);
    failure.summary = "required encrypted content could not be read or stored locally yet";
    failure
}

fn integrity_failure() -> HydrationFailureClass {
    HydrationFailureClass {
        state: MaterializationTaskState::Attention,
        kind: MaterializationFailureKind::ContentIntegrityFailed,
        summary: "encrypted content did not pass integrity verification",
        retry_policy: HydrationRetryPolicy::None,
    }
}

fn attention_failure(kind: MaterializationFailureKind) -> HydrationFailureClass {
    HydrationFailureClass {
        state: MaterializationTaskState::Attention,
        kind,
        summary: "encrypted content could not be verified and prepared safely",
        retry_policy: HydrationRetryPolicy::None,
    }
}

fn materialization_retry_not_before(
    now: &str,
    retry_key: &str,
    attempt_count: u32,
    policy: RetryBackoffPolicy,
) -> Result<String, SyncRunnerError> {
    let now = OffsetDateTime::parse(now, &Rfc3339).map_err(|_| {
        MetadataError::InvalidStorageMetadata(
            "materialization retry clock must be an RFC 3339 timestamp".to_string(),
        )
    })?;
    let delay_seconds =
        i64::try_from(policy.delay(retry_key, attempt_count).as_secs()).map_err(|_| {
            MetadataError::InvalidStorageMetadata(
                "materialization retry delay exceeded the timestamp range".to_string(),
            )
        })?;
    (now + TimeDuration::seconds(delay_seconds))
        .format(&Rfc3339)
        .map_err(|_| {
            MetadataError::InvalidStorageMetadata(
                "materialization retry timestamp could not be formatted".to_string(),
            )
            .into()
        })
}

#[cfg(test)]
#[path = "materialization_retry_tests.rs"]
mod tests;
