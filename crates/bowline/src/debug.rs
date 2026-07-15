use super::*;

/// Hidden `bowline debug classify <path>` affordance. Prints only the three
/// sync-load-bearing fields (classification / mode / access) using the shared
/// `classify_path` engine. No prose, no repair actions, not a versioned command,
/// and intentionally absent from public help and the command registry.
pub(super) fn print_debug_classify(args: DebugClassifyArgs, json: bool) -> ExitCode {
    let absolute_path = PathBuf::from(resolve_explicit_path(args.path));
    let metadata = std::fs::symlink_metadata(&absolute_path).ok();
    let is_dir = metadata.as_ref().is_some_and(|meta| meta.is_dir());
    let byte_len = metadata.as_ref().and_then(|meta| {
        if meta.is_dir() {
            None
        } else {
            Some(meta.len())
        }
    });

    let (relative_path, policy) = classify_inputs(&absolute_path);
    let decision = bowline_local::policy::classify_path(
        &bowline_local::policy::PathFacts {
            relative_path,
            is_dir,
            byte_len,
        },
        &policy,
    );

    let payload = serde_json::json!({
        "classification": decision.classification,
        "mode": decision.mode,
        "access": decision.access,
    });
    if json {
        print_json(&payload);
    } else {
        let access = decision
            .access
            .iter()
            .filter_map(|flag| serde_json::to_value(flag).ok())
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "classification: {}",
            payload["classification"].as_str().unwrap_or("unknown")
        );
        println!("mode: {}", payload["mode"].as_str().unwrap_or("unknown"));
        println!("access: {access}");
    }
    ExitCode::SUCCESS
}

fn classify_inputs(absolute_path: &Path) -> (String, bowline_local::policy::UserPolicy) {
    if let Some(root) = runtime::active_workspace_root() {
        let root_path = PathBuf::from(resolve_explicit_path(root));
        if let Ok(relative) = absolute_path.strip_prefix(&root_path) {
            let relative = relative.to_string_lossy().to_string();
            if let Ok(policy) =
                bowline_local::policy::UserPolicy::load_for_path(&root_path, &relative)
            {
                return (relative, policy);
            }
        }
    }
    let relative = absolute_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| absolute_path.to_string_lossy().to_string());
    (relative, bowline_local::policy::UserPolicy::empty())
}
