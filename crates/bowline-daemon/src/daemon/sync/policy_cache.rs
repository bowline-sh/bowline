use super::*;

pub(in crate::daemon) fn drain_policy<'cache>(
    root: &Path,
    relative_path: &str,
    policy_cache: &'cache mut HashMap<String, UserPolicy>,
) -> &'cache UserPolicy {
    policy_cache
        .entry(policy_cache_key(relative_path))
        .or_insert_with(|| {
            UserPolicy::load_for_path(root, relative_path).unwrap_or_else(|_| UserPolicy::empty())
        })
}

fn policy_cache_key(relative_path: &str) -> String {
    Path::new(relative_path)
        .parent()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

pub(in crate::daemon) fn invalidate_policy_cache_for_path(
    relative_path: &str,
    policy_cache: &mut HashMap<String, UserPolicy>,
) {
    if Path::new(relative_path).file_name() != Some(std::ffi::OsStr::new(".bowlineignore")) {
        return;
    }
    let key = policy_cache_key(relative_path);
    if key.is_empty() {
        policy_cache.clear();
        return;
    }
    let prefix = format!("{key}/");
    policy_cache.retain(|cached_key, _| cached_key != &key && !cached_key.starts_with(&prefix));
}
