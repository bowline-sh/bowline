use super::sync::{RootEntryKind, drain_policy, invalidate_policy_cache_for_path};
use super::*;
use bowline_core::git_paths::is_git_derivable_volatile_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatcherRuntimeState {
    Ready,
    Rearming,
    Limited(String),
}

#[derive(Debug)]
pub(super) enum WatcherSignal {
    Changed(Event),
    Recoverable,
    Limited(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct WatcherDrain {
    pub(super) changed: bool,
    pub(super) sync_now: bool,
}

#[derive(Debug, Default)]
pub(super) struct WatcherRecovery {
    pub(super) overflow_total: u64,
    pub(super) consecutive_overflows: u32,
    pub(super) last_overflow_at: Option<Instant>,
    pub(super) rearm_at: Option<Instant>,
    pub(super) rearm_failure_count: u32,
    pub(super) full_reconcile_required: bool,
}

pub(super) fn watcher_rearm_delay(consecutive_overflows: u32) -> Duration {
    let exponent = consecutive_overflows.saturating_sub(1).min(5);
    let multiplier = 1_u64 << exponent;
    Duration::from_secs(
        WATCHER_REARM_INITIAL
            .as_secs()
            .saturating_mul(multiplier)
            .min(WATCHER_REARM_MAX.as_secs()),
    )
}

pub(super) fn start_sync_watcher(
    root: &Path,
) -> Result<(RecommendedWatcher, Receiver<WatcherSignal>), notify::Error> {
    let (change_tx, change_rx) = mpsc::sync_channel(WATCHER_DRAIN_BUDGET);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        send_watcher_signal(&change_tx, event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, change_rx))
}

pub(super) fn send_watcher_signal(
    change_tx: &mpsc::SyncSender<WatcherSignal>,
    event: notify::Result<notify::Event>,
) {
    let signal = match event {
        Ok(event) => WatcherSignal::Changed(event),
        Err(error) if watcher_error_needs_rescan(&error) => WatcherSignal::Recoverable,
        Err(error) => WatcherSignal::Limited(error.to_string()),
    };
    match change_tx.try_send(signal) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            // One blocking overflow marker bounds retained history and makes
            // the next consumer collapse the entire backlog to a full scan.
            if let Err(error) = change_tx.send(WatcherSignal::Recoverable) {
                eprintln!("bowline-daemon watcher overflow marker dropped: {error}");
            }
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
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

pub(super) fn watcher_operation(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Create(_) => "create",
        EventKind::Remove(
            RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other,
        ) => "delete",
        EventKind::Modify(ModifyKind::Name(_)) => "rename",
        EventKind::Modify(ModifyKind::Metadata(_)) => "chmod",
        _ => "modify",
    }
}

pub(super) fn watcher_event_paths<'a>(
    root: &Path,
    operation: &str,
    event: &'a Event,
) -> Vec<(usize, &'a Path, Option<String>)> {
    if operation == "rename" && event.paths.len() >= 2 {
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

pub(super) fn is_private_state_path(path: &str) -> bool {
    path == ".bowline"
        || path.starts_with(".bowline/")
        || path == ".bowline-conflicts"
        || path.starts_with(".bowline-conflicts/")
}

// A rename's source is where a tracked file used to live. It must be rescanned
// when the file leaves — independent of whether the rename *destination* is
// recordable — or a scoped reconcile never observes the removal and the stale
// head-manifest entry survives, reappearing on the user's other machines.
// Returns the source path to mark dirty, or None when the source was never a
// synced location (non-rename event, empty, private state, or git-volatile).
pub(super) fn rename_source_dirty_path(source_path: Option<&str>) -> Option<&str> {
    let source = source_path?;
    if source.is_empty() || is_private_state_path(source) || is_git_derivable_volatile_path(source)
    {
        return None;
    }
    Some(source)
}

pub(super) fn stable_token(value: &str) -> String {
    let token = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    token.trim_matches('_').chars().take(80).collect()
}

impl ContinuousSyncRuntime {
    pub(super) fn drain_changes(&mut self) -> WatcherDrain {
        if self.change_rx.is_none() {
            return WatcherDrain::default();
        }
        let mut drain = WatcherDrain::default();
        // The policy cache is fresh per drain batch so .bowlineignore edits take
        // effect on the next drain without invalidation machinery.
        let mut policy_cache = HashMap::new();
        let mut consumed_count = 0;
        let mut rescan_required = false;
        for _ in 0..WATCHER_DRAIN_BUDGET {
            let Ok(signal) = self
                .change_rx
                .as_ref()
                .expect("receiver checked before drain")
                .try_recv()
            else {
                break;
            };
            consumed_count += 1;
            match signal {
                WatcherSignal::Changed(event) => {
                    if event.need_rescan() {
                        rescan_required = true;
                        break;
                    }
                    match self.record_watcher_event(&event, &mut policy_cache) {
                        Ok(true) => {
                            drain.changed = true;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            self.watcher_state = WatcherRuntimeState::Limited(error.to_string());
                            self.pending_dirty
                                .force_full(FullScanReason::WatcherUnavailable);
                            drain.sync_now = true;
                        }
                    }
                }
                WatcherSignal::Recoverable => {
                    self.pending_dirty
                        .force_full(FullScanReason::WatcherOverflow);
                    rescan_required = true;
                    break;
                }
                WatcherSignal::Limited(reason) => {
                    self.watcher_state = WatcherRuntimeState::Limited(reason);
                    self.pending_dirty
                        .force_full(FullScanReason::WatcherUnavailable);
                    drain.changed = true;
                    drain.sync_now = true;
                }
            }
        }
        let saturated = consumed_count == WATCHER_DRAIN_BUDGET
            && self.change_rx.as_ref().is_some_and(|change_rx| {
                drain_and_detect_sync_relevant_backlog(&self.options.args.root, change_rx)
            });
        if rescan_required || saturated {
            self.pending_dirty
                .force_full(FullScanReason::WatcherOverflow);
            self.begin_watcher_overflow_recovery(Instant::now());
            drain.changed = true;
            drain.sync_now = true;
        }
        drain
    }

    pub(super) fn begin_watcher_overflow_recovery(&mut self, now: Instant) {
        // Dropping the watcher stops the OS watch; dropping the receiver
        // discards the queued backlog. Safe: every reconcile tick rescans the
        // whole root, so dropped events cost latency, never correctness.
        self.watcher = None;
        self.change_rx = None;
        let recovery = &mut self.watcher_recovery;
        let storm_continues = recovery
            .last_overflow_at
            .is_some_and(|previous| now.duration_since(previous) <= WATCHER_OVERFLOW_RESET_WINDOW);
        recovery.consecutive_overflows = if storm_continues {
            recovery.consecutive_overflows.saturating_add(1)
        } else {
            1
        };
        recovery.overflow_total = recovery.overflow_total.saturating_add(1);
        recovery.last_overflow_at = Some(now);
        recovery.rearm_at = Some(now + watcher_rearm_delay(recovery.consecutive_overflows));
        recovery.full_reconcile_required = true;
        self.watcher_state = WatcherRuntimeState::Rearming;
    }

    pub(super) fn maybe_rearm_watcher(&mut self, now: Instant) -> bool {
        let Some(rearm_at) = self.watcher_recovery.rearm_at else {
            return false;
        };
        if now < rearm_at {
            return false;
        }
        match start_sync_watcher(&self.options.args.root) {
            Ok((watcher, change_rx)) => {
                self.watcher = Some(watcher);
                self.change_rx = Some(change_rx);
                self.watcher_recovery.rearm_at = None;
                self.watcher_recovery.rearm_failure_count = 0;
                // consecutive_overflows is reset only by the quiet window, not
                // by re-arm, so a storm cannot restart at the initial delay.
                self.watcher_state = WatcherRuntimeState::Ready;
                true
            }
            Err(error) => {
                let failures = self.watcher_recovery.rearm_failure_count.saturating_add(1);
                self.watcher_recovery.rearm_failure_count = failures;
                if failures >= WATCHER_REARM_FAILURE_LIMIT {
                    self.watcher_recovery.rearm_at = None;
                    self.watcher_state = WatcherRuntimeState::Limited(format!(
                        "watcher re-arm failed {failures} times: {error}"
                    ));
                    true
                } else {
                    self.watcher_recovery.rearm_at = Some(now + watcher_rearm_delay(failures));
                    false
                }
            }
        }
    }

    pub(super) fn record_watcher_event(
        &mut self,
        event: &Event,
        policy_cache: &mut HashMap<String, UserPolicy>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let operation = watcher_operation(&event.kind);
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let now = current_timestamp();
        let causation_id = format!("watch_{}_{}", self.tick_count, stable_token(&now));

        let paths = watcher_event_paths(&self.options.args.root, operation, event);
        let mut recorded = false;
        let mut dirty_paths: Vec<(String, RootEntryKind)> = Vec::new();
        self.store.with_store(|store| {
            for (index, path, source_path) in paths {
                // Dirty the rename source before the destination filters below:
                // moving a tracked file to a filtered location (git-volatile,
                // out-of-root, non-syncing) still removes it from the source,
                // which a scoped reconcile must rescan. The source path is gone,
                // so it carries no metadata: route it as `Unknown`, never by
                // assuming the destination's kind.
                if let Some(source) = rename_source_dirty_path(source_path.as_deref()) {
                    dirty_paths.push((source.to_string(), RootEntryKind::Unknown));
                    recorded = true;
                }
                let Some(relative_path) = watcher_relative_path(&self.options.args.root, path)
                else {
                    continue;
                };
                if relative_path.is_empty() || is_private_state_path(&relative_path) {
                    continue;
                }
                if is_git_derivable_volatile_path(&relative_path) {
                    continue;
                }
                invalidate_policy_cache_for_path(&relative_path, policy_cache);
                let metadata = fs::symlink_metadata(path).ok();
                let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
                // Tri-state entry kind for root routing: present-and-dir vs
                // present-and-file-like (regular file OR symlink) vs vanished
                // (deletion / rename-away / stat failure).
                let entry_kind = match metadata.as_ref() {
                    Some(metadata) if metadata.is_dir() => RootEntryKind::Directory,
                    Some(_) => RootEntryKind::NonDirectory,
                    None => RootEntryKind::Unknown,
                };
                let byte_len = metadata
                    .as_ref()
                    .filter(|metadata| !metadata.is_dir())
                    .map(|metadata| metadata.len());
                let policy = drain_policy(&self.options.args.root, &relative_path, policy_cache);
                let decision = classify_path(
                    &PathFacts {
                        relative_path: relative_path.clone(),
                        is_dir,
                        byte_len,
                    },
                    policy,
                );
                if !watcher_should_record(decision.classification, decision.mode) {
                    continue;
                }
                dirty_paths.push((relative_path.clone(), entry_kind));
                store.append_local_write_log(&LocalWriteLogRecord {
                    id: format!(
                        "watch_{}_{}_{}",
                        stable_token(&relative_path),
                        stable_token(operation),
                        stable_token(&format!("{now}-{index}")),
                    ),
                    workspace_id: workspace_id.clone(),
                    device_id: device_id.clone(),
                    project_id: None,
                    path: relative_path,
                    source_path,
                    operation: operation.to_string(),
                    staged_content_id: None,
                    policy_classification: decision.classification,
                    causation_id: causation_id.clone(),
                    settled_at: now.clone(),
                    created_at: now.clone(),
                })?;
                recorded = true;
            }
            Ok(())
        })?;
        for (path, kind) in dirty_paths {
            self.pending_dirty.insert_path_parent(&path, kind);
        }
        Ok(recorded)
    }

    pub(super) fn watcher_state_json(&self) -> WatcherRuntimeStateJson<'_> {
        WatcherRuntimeStateJson::from_state(&self.watcher_state, &self.watcher_recovery)
    }

    pub(super) fn watcher_component_state(&self) -> &'static str {
        match self.watcher_state {
            WatcherRuntimeState::Ready => "ready",
            WatcherRuntimeState::Rearming | WatcherRuntimeState::Limited(_) => "degraded",
        }
    }
}

fn drain_and_detect_sync_relevant_backlog(
    root: &Path,
    change_rx: &Receiver<WatcherSignal>,
) -> bool {
    // This destructively drains the saturated queue. The sole caller converts a
    // positive result into WatcherOverflow, whose full rescan re-observes every
    // path represented by the discarded signals.
    while let Ok(signal) = change_rx.try_recv() {
        if watcher_signal_is_sync_relevant(root, &signal) {
            return true;
        }
    }
    false
}

// Whether one watcher path tuple warrants a sync. Mirrors what
// `record_watcher_event` would act on: a recordable destination OR a rename
// whose source is a tracked location. The saturated-drain backlog check must
// consider the source too, or a rename of a tracked file to a filtered
// destination is discarded under event storms and its deletion is lost.
fn watcher_path_tuple_is_sync_relevant(
    root: &Path,
    path: &Path,
    source_path: Option<&str>,
) -> bool {
    let destination_relevant = watcher_relative_path(root, path).is_some_and(|relative_path| {
        !relative_path.is_empty()
            && !is_private_state_path(&relative_path)
            && !is_git_derivable_volatile_path(&relative_path)
    });
    destination_relevant || rename_source_dirty_path(source_path).is_some()
}

fn watcher_signal_is_sync_relevant(root: &Path, signal: &WatcherSignal) -> bool {
    match signal {
        WatcherSignal::Changed(event) => {
            event.need_rescan()
                || watcher_event_paths(root, watcher_operation(&event.kind), event)
                    .into_iter()
                    .any(|(_, path, source_path)| {
                        watcher_path_tuple_is_sync_relevant(root, path, source_path.as_deref())
                    })
        }
        WatcherSignal::Recoverable | WatcherSignal::Limited(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{rename_source_dirty_path, watcher_path_tuple_is_sync_relevant};
    use bowline_core::git_paths::is_git_derivable_volatile_path;
    use std::path::Path;

    #[test]
    fn watcher_git_churn_predicate_skips_derivable_state_only() {
        assert!(!is_git_derivable_volatile_path("repo/.git/index"));
        assert!(is_git_derivable_volatile_path("repo/.git/logs"));
        assert!(!is_git_derivable_volatile_path("repo/.git/HEAD"));
    }

    #[test]
    fn rename_source_is_dirtied_even_when_destination_is_filtered() {
        // A tracked file moved anywhere must mark its source dirty so a scoped
        // reconcile drops the stale entry, regardless of the destination.
        assert_eq!(
            rename_source_dirty_path(Some("src/app.rs")),
            Some("src/app.rs")
        );
        // Non-rename events carry no source; nothing extra to dirty.
        assert_eq!(rename_source_dirty_path(None), None);
        // Sources that were never synced need no rescan.
        assert_eq!(rename_source_dirty_path(Some("")), None);
        assert_eq!(rename_source_dirty_path(Some(".bowline/state.json")), None);
        assert_eq!(
            rename_source_dirty_path(Some("repo/.git/index")),
            Some("repo/.git/index")
        );
    }

    #[test]
    fn saturated_backlog_treats_rename_source_as_relevant() {
        let root = Path::new("/ws");
        // Destination outside the workspace, but the source is a tracked file:
        // the backlog check must keep it sync-relevant so a saturated drain
        // forces a rescan instead of dropping the source deletion.
        assert!(watcher_path_tuple_is_sync_relevant(
            root,
            Path::new("/elsewhere/app.rs"),
            Some("src/app.rs"),
        ));
        // Neither destination nor source is a synced location.
        assert!(watcher_path_tuple_is_sync_relevant(
            root,
            Path::new("/elsewhere/app.rs"),
            Some("repo/.git/index"),
        ));
        // Non-rename event with a recordable destination stays relevant.
        assert!(watcher_path_tuple_is_sync_relevant(
            root,
            Path::new("/ws/src/app.rs"),
            None,
        ));
    }
}
