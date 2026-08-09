use std::collections::HashMap;
use std::fs;
use std::path::Path;

use bowline_core::git_paths::is_git_derivable_volatile_path;
use bowline_local::policy::{
    PathFacts, UserPolicy, classify_path, is_private_workspace_state_path,
    is_work_view_namespace_path,
};

use super::{
    drain_policy, invalidate_policy_cache_for_path, watcher_relative_path, watcher_should_record,
};

pub(super) struct WatcherDestination {
    pub(super) relative_path: String,
}

pub(super) fn watcher_destination(
    root: &Path,
    path: &Path,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> Option<WatcherDestination> {
    let relative_path = watcher_relative_path(root, path)?;
    if relative_path.is_empty()
        || is_private_workspace_state_path(&relative_path)
        || is_work_view_namespace_path(&relative_path)
        || is_git_derivable_volatile_path(&relative_path)
    {
        return None;
    }
    invalidate_policy_cache_for_path(&relative_path, policy_cache);
    let metadata = fs::symlink_metadata(path).ok();
    let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
    let byte_len = metadata
        .as_ref()
        .filter(|metadata| !metadata.is_dir())
        .map(|metadata| metadata.len());
    let policy = drain_policy(root, &relative_path, policy_cache);
    let decision = classify_path(
        &PathFacts {
            relative_path: relative_path.clone(),
            is_dir,
            byte_len,
        },
        policy,
    );
    watcher_should_record(decision.classification, decision.mode)
        .then_some(WatcherDestination { relative_path })
}
