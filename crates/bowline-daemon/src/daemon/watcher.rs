use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatcherRuntimeState {
    Ready,
    Limited(String),
}

#[derive(Debug)]
pub(super) enum WatcherSignal {
    Changed(Event),
    Limited(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct WatcherDrain {
    pub(super) changed: bool,
    pub(super) sync_now: bool,
}

pub(super) fn start_sync_watcher(
    root: &Path,
) -> Result<(RecommendedWatcher, Receiver<WatcherSignal>), notify::Error> {
    let (change_tx, change_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        send_watcher_signal(&change_tx, event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, change_rx))
}

pub(super) fn send_watcher_signal(
    change_tx: &Sender<WatcherSignal>,
    event: notify::Result<notify::Event>,
) {
    match event {
        Ok(event) => {
            let _ = change_tx.send(WatcherSignal::Changed(event));
        }
        Err(error) => {
            let _ = change_tx.send(WatcherSignal::Limited(error.to_string()));
        }
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
