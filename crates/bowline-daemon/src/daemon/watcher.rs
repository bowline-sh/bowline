use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bowline_core::git_paths::{is_git_derivable_volatile_path, is_git_directory_path};
use bowline_core::policy::{MaterializationMode, PathClassification};
use bowline_core::workspace_graph::normalize_workspace_path;
use bowline_daemon::watcher_coverage::{
    CoverageCancellation, CoverageWait, NativeEventHandler, NativeWatcherCoverageAdapter,
    WatcherCoverageAdapter, WatcherCoverageError, WatcherCoverageHandoff, WatcherCoverageIds,
    WatcherCoveragePreparation, start_native_adapter,
};
use bowline_daemon::watcher_recovery::{RecoveryCause, WatcherRecoveryCoordinator};
use bowline_local::policy::{
    PathFacts, UserPolicy, classify_path, is_private_workspace_state_path,
    is_work_view_namespace_path, policy_should_recurse,
};
use notify::Event;
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RemoveKind};

use super::sync::{RecoveryClock, drain_policy, invalidate_policy_cache_for_path};
use crate::daemon::WATCHER_DRAIN_BUDGET;

#[path = "watcher/destination.rs"]
mod destination;
use destination::watcher_destination;

const WATCHER_COVERAGE_START_TIMEOUT: Duration = Duration::from_secs(30);

/// A watcher-kernel signal. The overflow lane is installed once ahead of native
/// events, then retained by the bridge as out-of-band recovery state.
#[derive(Debug)]
pub(super) enum WatcherSignal {
    Changed { event: Event },
    Recoverable,
    Limited { reason: String },
    OverflowLane(Arc<WatcherOverflowLane>),
}

/// A level-triggered overflow request shared by the native callback and bridge.
/// The callback only sets the bit; it never waits for channel capacity.
#[derive(Debug, Default)]
pub(super) struct WatcherOverflowLane {
    recovery_requested: AtomicBool,
}

impl WatcherOverflowLane {
    pub(super) fn request_recovery(&self) {
        self.recovery_requested.store(true, Ordering::Release);
    }

    pub(super) fn recovery_requested(&self) -> bool {
        self.recovery_requested.load(Ordering::Acquire)
    }

    pub(super) fn take_recovery_request(&self) -> bool {
        self.recovery_requested.swap(false, Ordering::AcqRel)
    }
}

/// The workspace filesystem watcher kernel: a recursive notify watch on the
/// workspace root whose callback read-filters events into [`WatcherSignal`]s.
/// Dropping it tears down the native watch.
pub(in crate::daemon) struct SyncWatcher {
    coverage: Arc<Mutex<NativeWatcherCoverageAdapter>>,
}

#[derive(Clone)]
pub(in crate::daemon) struct SyncWatcherCoverageHandle {
    coverage: Arc<Mutex<NativeWatcherCoverageAdapter>>,
}

impl SyncWatcher {
    pub(in crate::daemon) fn coverage_handle(&self) -> SyncWatcherCoverageHandle {
        SyncWatcherCoverageHandle {
            coverage: Arc::clone(&self.coverage),
        }
    }
}

#[cfg(test)]
pub(in crate::daemon) fn start_sync_watcher(
    root: &Path,
    ids: WatcherCoverageIds,
) -> Result<(SyncWatcher, Receiver<WatcherSignal>), WatcherCoverageError> {
    start_sync_watcher_internal(root, ids, None)
}

pub(in crate::daemon) fn start_sync_watcher_with_recovery(
    root: &Path,
    ids: WatcherCoverageIds,
    recovery: Arc<WatcherRecoveryCoordinator>,
    recovery_clock: Arc<RecoveryClock>,
) -> Result<(SyncWatcher, Receiver<WatcherSignal>), WatcherCoverageError> {
    start_sync_watcher_internal(
        root,
        ids,
        Some(WatcherRecoveryIngress {
            coordinator: recovery,
            clock: recovery_clock,
        }),
    )
}

#[derive(Clone)]
struct WatcherRecoveryIngress {
    coordinator: Arc<WatcherRecoveryCoordinator>,
    clock: Arc<RecoveryClock>,
}

impl WatcherRecoveryIngress {
    fn observe_activity(&self) {
        let _admission = self.coordinator.observe_activity(self.clock.now());
    }

    fn observe_suppressed(&self) {
        let _admission = self.coordinator.observe_suppressed(self.clock.now());
    }

    fn observe_loss(&self, cause: RecoveryCause) {
        let _admission = self.coordinator.observe_loss(cause, self.clock.now());
    }
}

fn start_sync_watcher_internal(
    root: &Path,
    ids: WatcherCoverageIds,
    recovery: Option<WatcherRecoveryIngress>,
) -> Result<(SyncWatcher, Receiver<WatcherSignal>), WatcherCoverageError> {
    let (change_tx, change_rx) = mpsc::sync_channel(WATCHER_DRAIN_BUDGET);
    let overflow_lane = Arc::new(WatcherOverflowLane::default());
    change_tx
        .try_send(WatcherSignal::OverflowLane(Arc::clone(&overflow_lane)))
        .expect("a new watcher channel has room for its overflow lane");
    let callback_tx = change_tx.clone();
    let callback_overflow_lane = Arc::clone(&overflow_lane);
    let reported_root = root.to_path_buf();
    let watch_root = fs::canonicalize(root).unwrap_or_else(|_| reported_root.clone());
    let callback_watch_root = watch_root.clone();
    let handler: NativeEventHandler =
        Arc::new(move |_epoch, mut event: notify::Result<notify::Event>| {
            if let Ok(event) = &mut event {
                remap_watcher_event_root(event, &callback_watch_root, &reported_root);
            }
            send_watcher_signal_with_recovery(
                &callback_tx,
                &callback_overflow_lane,
                event,
                recovery.as_ref(),
            );
        });
    let wait = CoverageWait::new(
        Instant::now() + WATCHER_COVERAGE_START_TIMEOUT,
        CoverageCancellation::new(),
    );
    let coverage = start_native_adapter(&watch_root, handler, ids, &wait)?;
    Ok((
        SyncWatcher {
            coverage: Arc::new(Mutex::new(coverage)),
        },
        change_rx,
    ))
}

impl std::fmt::Debug for SyncWatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyncWatcher")
            .field("coverage", &"native")
            .finish()
    }
}

impl WatcherCoverageAdapter for SyncWatcher {
    fn begin_recovery(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .begin_recovery(wait)
    }

    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .seal_after_scan(preparation, wait)
    }

    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .validate_boundary(handoff)
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .shutdown()
    }
}

impl WatcherCoverageAdapter for SyncWatcherCoverageHandle {
    fn begin_recovery(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .begin_recovery(wait)
    }

    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .seal_after_scan(preparation, wait)
    }

    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .validate_boundary(handoff)
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        self.coverage
            .lock()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?
            .shutdown()
    }
}

fn remap_watcher_event_root(event: &mut Event, watched_root: &Path, reported_root: &Path) {
    for path in &mut event.paths {
        if let Ok(relative) = path.strip_prefix(watched_root) {
            *path = reported_root.join(relative);
        }
    }
}

#[cfg(test)]
pub(super) fn send_watcher_signal(
    change_tx: &mpsc::SyncSender<WatcherSignal>,
    overflow_lane: &WatcherOverflowLane,
    event: notify::Result<notify::Event>,
) {
    send_watcher_signal_with_recovery(change_tx, overflow_lane, event, None);
}

fn send_watcher_signal_with_recovery(
    change_tx: &mpsc::SyncSender<WatcherSignal>,
    overflow_lane: &WatcherOverflowLane,
    event: notify::Result<notify::Event>,
    recovery: Option<&WatcherRecoveryIngress>,
) {
    let signal = match event {
        Ok(event) if event.need_rescan() => {
            if let Some(recovery) = recovery {
                recovery.observe_loss(RecoveryCause::NativeRescanRequired);
            }
            WatcherSignal::Changed { event }
        }
        Ok(event) if watcher_operation(&event.kind).is_none() => return,
        Ok(event) if overflow_lane.recovery_requested() => {
            // The latch is asserted, so this event is about to be dropped below.
            // Admit the drop as lost fidelity before returning: that admission is
            // the only record the write ever happened, and it is what stops the
            // incident closing over it. Ordering is safe either way -- an
            // admission landing before `offer_close` invalidates it, and one
            // landing after closure opens a fresh incident that must rescan.
            if let Some(recovery) = recovery {
                recovery.observe_suppressed();
            }
            WatcherSignal::Changed { event }
        }
        Ok(event) => {
            // Forwarded activity is durable in the ingress and does not gate the
            // close; it still advances the activity frontier that exact barriers
            // fence on.
            if let Some(recovery) = recovery {
                recovery.observe_activity();
            }
            WatcherSignal::Changed { event }
        }
        Err(error) if watcher_error_needs_rescan(&error) => {
            if let Some(recovery) = recovery {
                recovery.observe_loss(RecoveryCause::NativeRescanRequired);
            }
            WatcherSignal::Recoverable
        }
        Err(error) => WatcherSignal::Limited {
            reason: {
                if let Some(recovery) = recovery {
                    recovery.observe_loss(RecoveryCause::RecoverableAdapterLoss);
                }
                error.to_string()
            },
        },
    };
    if overflow_lane.recovery_requested()
        && matches!(
            &signal,
            WatcherSignal::Changed { .. } | WatcherSignal::Recoverable
        )
    {
        return;
    }
    match change_tx.try_send(signal) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            if let Some(recovery) = recovery {
                recovery.observe_loss(RecoveryCause::NativeCallbackLaneSaturated);
            }
            overflow_lane.request_recovery();
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            if let Some(recovery) = recovery {
                recovery.observe_loss(RecoveryCause::WatcherDisconnected);
            }
            eprintln!("bowline-daemon watcher signal receiver disconnected");
        }
    }
}

fn watcher_error_needs_rescan(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::Generic(reason) => {
            let normalized = reason.to_ascii_lowercase();
            normalized.contains("overflow") || normalized.contains("rescan")
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherOperation {
    Create,
    Delete,
    Rename,
    Metadata,
    Modify,
}

fn watcher_operation(kind: &EventKind) -> Option<WatcherOperation> {
    match kind {
        EventKind::Access(
            AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read),
        ) => None,
        EventKind::Create(_) => Some(WatcherOperation::Create),
        EventKind::Remove(
            RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other,
        ) => Some(WatcherOperation::Delete),
        EventKind::Modify(ModifyKind::Name(_)) => Some(WatcherOperation::Rename),
        EventKind::Modify(ModifyKind::Metadata(_)) => Some(WatcherOperation::Metadata),
        _ => Some(WatcherOperation::Modify),
    }
}

fn watcher_event_paths<'a>(
    root: &Path,
    operation: WatcherOperation,
    event: &'a Event,
) -> Vec<(usize, &'a Path, Option<String>)> {
    if operation == WatcherOperation::Rename && event.paths.len() >= 2 {
        return vec![(
            1,
            event.paths[1].as_path(),
            watcher_relative_path(root, &event.paths[0]),
        )];
    }
    event
        .paths
        .iter()
        .enumerate()
        .map(|(index, path)| (index, path.as_path(), None))
        .collect()
}

pub(super) fn watcher_relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) if path.is_absolute() => return None,
        Err(_) => path,
    };
    let normalized = normalize_workspace_path(&relative.display().to_string());
    if normalized.starts_with("..") {
        return None;
    }
    Some(normalized)
}

/// Translate one watcher signal into an engine event (Plan 111 Step 1b),
/// preserving the watcher kernel's read/private/git filtering. A recordable
/// change yields `Paths`; a lost-fidelity signal (overflow, adapter loss) yields
/// `FullScanRequired`, which the engine recovers with one cheap stat walk.
pub(in crate::daemon) fn watcher_signal_engine_event(
    root: &Path,
    signal: &WatcherSignal,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> Option<bowline_local::sync::manifest_engine::EngineEvent> {
    use bowline_local::sync::manifest_engine::{EngineEvent, FullScanReason};
    match signal {
        WatcherSignal::Changed { event } if event.need_rescan() => Some(
            EngineEvent::FullScanRequired(FullScanReason::WatcherOverflow),
        ),
        WatcherSignal::Changed { event } => {
            let recursive_roots = watcher_event_recursive_roots(root, event, policy_cache);
            if !recursive_roots.is_empty() {
                return Some(EngineEvent::RecursivePaths(recursive_roots));
            }
            let paths = watcher_event_engine_paths(root, event, policy_cache);
            (!paths.is_empty()).then_some(EngineEvent::Paths(paths))
        }
        WatcherSignal::Recoverable => Some(EngineEvent::FullScanRequired(
            FullScanReason::WatcherOverflow,
        )),
        WatcherSignal::Limited { reason } => {
            eprintln!("bowline-daemon watcher adapter is unavailable: {reason}");
            Some(EngineEvent::FullScanRequired(
                FullScanReason::WatcherDisconnected,
            ))
        }
        WatcherSignal::OverflowLane(_) => None,
    }
}

fn watcher_event_recursive_roots(
    root: &Path,
    event: &Event,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> std::collections::BTreeSet<bowline_local::sync::manifest_engine::WorkspacePath> {
    use bowline_local::sync::manifest_engine::WorkspacePath;
    let mut roots = std::collections::BTreeSet::new();
    let Some(operation) = watcher_operation(&event.kind) else {
        return roots;
    };
    let recursive_without_metadata = operation == WatcherOperation::Delete
        || matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(
                notify::event::RenameMode::From | notify::event::RenameMode::Any
            ))
        );
    for (_, path, source_path) in watcher_event_paths(root, operation, event) {
        let destination_is_directory =
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
        if !recursive_without_metadata && !destination_is_directory {
            continue;
        }
        if let Some(source) = source_path
            && watcher_recursive_root(root, &source, policy_cache)
        {
            roots.insert(WorkspacePath::new(source));
        }
        if let Some(relative) = watcher_relative_path(root, path)
            && watcher_recursive_root(root, &relative, policy_cache)
        {
            roots.insert(WorkspacePath::new(relative));
        }
    }
    roots
}

fn watcher_recursive_root(
    root: &Path,
    relative_path: &str,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> bool {
    if relative_path.is_empty()
        || is_private_workspace_state_path(relative_path)
        || is_work_view_namespace_path(relative_path)
    {
        return false;
    }
    if is_git_derivable_volatile_path(relative_path) {
        return is_git_directory_path(relative_path);
    }
    invalidate_policy_cache_for_path(relative_path, policy_cache);
    let absolute = root.join(relative_path);
    let metadata = fs::symlink_metadata(&absolute).ok();
    let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
    let byte_len = metadata
        .as_ref()
        .filter(|metadata| !metadata.is_dir())
        .map(|metadata| metadata.len());
    let policy = drain_policy(root, relative_path, policy_cache);
    let decision = classify_path(
        &PathFacts {
            relative_path: relative_path.to_string(),
            is_dir,
            byte_len,
        },
        policy,
    );
    policy_should_recurse(&decision, policy, relative_path)
}

/// The recordable workspace paths a watcher event touches, read-filtered by the
/// same policy classification the old journal path used. A rename dirties both
/// its source (so the stale entry drops) and its recordable destination.
fn watcher_event_engine_paths(
    root: &Path,
    event: &Event,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> std::collections::BTreeSet<bowline_local::sync::manifest_engine::WorkspacePath> {
    use bowline_local::sync::manifest_engine::WorkspacePath;
    let mut paths = std::collections::BTreeSet::new();
    let Some(operation) = watcher_operation(&event.kind) else {
        return paths;
    };
    for (_, path, source_path) in watcher_event_paths(root, operation, event) {
        if let Some(source) = rename_source_dirty_path(source_path.as_deref()) {
            paths.insert(WorkspacePath::new(source.to_string()));
        }
        if let Some(destination) = watcher_destination(root, path, policy_cache) {
            paths.insert(WorkspacePath::new(destination.relative_path));
        }
    }
    paths
}

pub(super) fn watcher_should_record(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::ProjectEnv, _)
            | (PathClassification::SecretLooking, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
    )
}

// A rename's source is where a tracked file used to live. It must be rescanned
// when the file leaves — independent of whether the rename *destination* is
// recordable — or a scoped reconcile never observes the removal and the stale
// head-manifest entry survives, reappearing on the user's other machines.
// Returns the source path to mark dirty, or None when the source was never a
// synced location (non-rename event, empty, private state, or git-volatile).
pub(super) fn rename_source_dirty_path(source_path: Option<&str>) -> Option<&str> {
    let source = source_path?;
    if source.is_empty()
        || is_private_workspace_state_path(source)
        || is_work_view_namespace_path(source)
        || is_git_derivable_volatile_path(source)
    {
        return None;
    }
    Some(source)
}

#[cfg(test)]
mod tests;
