use std::{fs, path::PathBuf};

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkCleanupCommandOutput},
    events::EventName,
    status::{RepairCommand, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkViewLifecycle, WorkViewRetentionState, WorkViewVisibility,
    },
};

use super::{
    WorkCleanupOptions, WorkViewError, materialization_snapshot,
    paths::{
        acquire_work_view_transition_lock, append_workspace_event, display_path,
        ensure_existing_path_inside_real, ensure_no_symlink_ancestors, ensure_path_inside,
        expand_display_path, open_store, work_namespace_root,
    },
    status_all_command,
};

pub fn cleanup_work_views(
    options: WorkCleanupOptions,
) -> Result<WorkCleanupCommandOutput, WorkViewError> {
    #[cfg(not(unix))]
    if options.apply {
        return Err(WorkViewError::ProjectWriterBusy {
            project_path: "work cleanup".to_string(),
            reason: "destructive cleanup is unavailable on this platform because Bowline cannot \
                     prove that the materialization has no open writers"
                .to_string(),
        });
    }
    let store = open_store(options.db_path.as_deref())?;
    let _transition_lock = acquire_work_view_transition_lock(&store)?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let mut candidates = store
        .work_views(&workspace.id, true, None)?
        .into_iter()
        .filter(|view| {
            matches!(
                view.lifecycle,
                WorkViewLifecycle::Accepted | WorkViewLifecycle::Discarded
            ) && !matches!(view.retention.state, WorkViewRetentionState::DeleteEligible)
        })
        .collect::<Vec<_>>();
    let previewed_paths = candidates
        .iter()
        .flat_map(|view| view.host_materializations.iter().cloned())
        .collect::<Vec<_>>();
    let mut deleted_paths = Vec::new();
    if options.apply {
        let retired = quarantine_all(&store, &candidates, &options.expected_materializations)?;
        deleted_paths = retired
            .iter()
            .map(|materialization| materialization.display.clone())
            .collect();
        for view in &mut candidates {
            // Cleanup is a terminal scrub, not a lifecycle transition: remove
            // materializations and mark the row delete-eligible while preserving
            // whether the work was accepted or discarded.
            view.visibility = WorkViewVisibility::Hidden;
            view.retention.state = WorkViewRetentionState::DeleteEligible;
            view.retention.retain_until = None;
            view.retention.restorable = false;
            view.updated_at = options.generated_at.clone();
        }
        if let Err(error) = store.upsert_work_views(&candidates) {
            rollback_retired(&retired)?;
            return Err(error.into());
        }
        for (index, materialization) in retired.iter().enumerate() {
            let removal = (|| -> Result<(), WorkViewError> {
                #[cfg(unix)]
                make_directories_writable(&materialization.quarantine)?;
                fs::remove_dir_all(&materialization.quarantine)?;
                Ok(())
            })();
            if let Err(error) = removal {
                rollback_retired(&retired[index + 1..])?;
                mark_partial_cleanup_metadata(&mut candidates, &retired, index);
                store.upsert_work_views(&candidates)?;
                return Err(error);
            }
        }
        append_workspace_event(
            &store,
            EventName::WorkCleanupCompleted,
            &workspace.id,
            &options.generated_at,
            "Cleaned up retained work views",
        );
    } else {
        append_workspace_event(
            &store,
            EventName::WorkCleanupPreviewed,
            &workspace.id,
            &options.generated_at,
            "Previewed retained work-view cleanup",
        );
    }

    let status_command = status_all_command(&store, &workspace.id)?;
    Ok(WorkCleanupCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Cleanup,
        generated_at: options.generated_at,
        action: if options.apply {
            WorkCommandAction::CleanupApplied
        } else {
            WorkCommandAction::CleanupPreviewed
        },
        workspace_id: workspace.id,
        previewed_paths,
        deleted_paths,
        unresolved_paths: Vec::new(),
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "List retained work views".to_string(),
            Some(status_command),
        )],
    })
}

struct RetiredMaterialization {
    view_id: String,
    original: PathBuf,
    quarantine: PathBuf,
    display: String,
    already_quarantined: bool,
    #[cfg(unix)]
    original_modes: Vec<(PathBuf, u32)>,
}

fn quarantine_all(
    store: &crate::metadata::MetadataStore,
    candidates: &[bowline_core::work_views::WorkView],
    expected_materializations: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<RetiredMaterialization>, WorkViewError> {
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let mut retired = Vec::new();
    for view in candidates {
        match quarantine_view(store, view, &workspace_root, expected_materializations) {
            Ok(view_retired) => retired.extend(view_retired),
            Err(error) => {
                rollback_retired(&retired)?;
                return Err(error);
            }
        }
    }
    Ok(retired)
}

fn quarantine_view(
    store: &crate::metadata::MetadataStore,
    view: &bowline_core::work_views::WorkView,
    workspace_root: &std::path::Path,
    expected_materializations: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<RetiredMaterialization>, WorkViewError> {
    let namespace_root =
        work_namespace_root(store, view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
    ensure_no_symlink_ancestors(
        &namespace_root,
        workspace_root,
        "cleanup namespace escapes .work",
    )?;
    let mut retired = Vec::new();
    for (index, display) in view.host_materializations.iter().enumerate() {
        match quarantine_one(
            view,
            index,
            display,
            &namespace_root,
            expected_materializations,
        ) {
            Ok(Some(materialization)) => retired.push(materialization),
            Ok(None) => {}
            Err(error) => {
                rollback_retired(&retired)?;
                return Err(error);
            }
        }
    }
    Ok(retired)
}

fn quarantine_one(
    view: &bowline_core::work_views::WorkView,
    index: usize,
    display: &str,
    namespace_root: &std::path::Path,
    expected_materializations: &std::collections::BTreeMap<String, String>,
) -> Result<Option<RetiredMaterialization>, WorkViewError> {
    let expected = expected_materializations.get(display).ok_or_else(|| {
        WorkViewError::MaterializationChangedAfterReview {
            path: display.to_string(),
        }
    })?;
    let original = expand_display_path(display);
    ensure_path_inside(&original, namespace_root, "cleanup is limited to .work")?;
    ensure_no_symlink_ancestors(&original, namespace_root, "cleanup target escapes .work")?;
    if !original.exists() {
        return Err(WorkViewError::MaterializationChangedAfterReview {
            path: display.to_string(),
        });
    }
    if original
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.starts_with(".bowline-cleanup-"))
    {
        let actual = materialization_snapshot(&original)?;
        if &actual != expected {
            return Err(WorkViewError::MaterializationChangedAfterReview {
                path: display.to_string(),
            });
        }
        #[cfg(unix)]
        let original_modes = seal_and_revalidate(&original, expected, display)?;
        return Ok(Some(RetiredMaterialization {
            view_id: view.id.as_str().to_string(),
            original: original.clone(),
            quarantine: original,
            display: display.to_string(),
            already_quarantined: true,
            #[cfg(unix)]
            original_modes,
        }));
    }
    ensure_existing_path_inside_real(&original, namespace_root, "cleanup target escapes .work")?;
    let quarantine = namespace_root.join(format!(".bowline-cleanup-{}-{index}", view.id.as_str()));
    if quarantine.exists() {
        return Err(WorkViewError::MaterializationChangedAfterReview {
            path: display.to_string(),
        });
    }
    fs::rename(&original, &quarantine)?;
    let actual = match materialization_snapshot(&quarantine) {
        Ok(actual) => actual,
        Err(error) => {
            restore_quarantine(&original, &quarantine)?;
            return Err(error);
        }
    };
    if &actual != expected {
        restore_quarantine(&original, &quarantine)?;
        return Err(WorkViewError::MaterializationChangedAfterReview {
            path: display.to_string(),
        });
    }
    #[cfg(unix)]
    let original_modes = match seal_and_revalidate(&quarantine, expected, display) {
        Ok(modes) => modes,
        Err(error) => {
            restore_quarantine(&original, &quarantine)?;
            return Err(error);
        }
    };
    Ok(Some(RetiredMaterialization {
        view_id: view.id.as_str().to_string(),
        original,
        quarantine,
        display: display_path(&expand_display_path(display)),
        already_quarantined: false,
        #[cfg(unix)]
        original_modes,
    }))
}

#[cfg(unix)]
fn seal_and_revalidate(
    quarantine: &std::path::Path,
    expected: &str,
    display: &str,
) -> Result<Vec<(PathBuf, u32)>, WorkViewError> {
    let modes = seal_quarantine(quarantine, display)?;
    let sealed = match materialization_snapshot(quarantine) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            restore_tree_modes(&modes)?;
            return Err(error);
        }
    };
    if sealed != expected {
        restore_tree_modes(&modes)?;
        return Err(WorkViewError::MaterializationChangedAfterReview {
            path: display.to_string(),
        });
    }
    // This boundary protects against unaware concurrent editors and agents.
    // A same-UID process that deliberately discovers this hidden path and
    // changes its permissions can already delete the original work view
    // directly; unprivileged code cannot sandbox that actor.
    Ok(modes)
}

fn mark_partial_cleanup_metadata(
    candidates: &mut [bowline_core::work_views::WorkView],
    retired: &[RetiredMaterialization],
    failed_index: usize,
) {
    for view in candidates {
        let remaining = retired
            .iter()
            .enumerate()
            .filter(|(index, materialization)| {
                *index >= failed_index
                    && materialization.view_id == view.id.as_str()
                    && if *index == failed_index {
                        materialization.quarantine.exists()
                    } else {
                        materialization.original.exists()
                    }
            })
            .map(|(index, materialization)| {
                if index == failed_index {
                    display_path(&materialization.quarantine)
                } else {
                    materialization.display.clone()
                }
            })
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            continue;
        }
        let has_partial = retired.get(failed_index).is_some_and(|materialization| {
            materialization.view_id == view.id.as_str() && materialization.quarantine.exists()
        });
        view.host_materializations = remaining;
        view.retention.state = WorkViewRetentionState::Retained;
        view.retention.restorable = !has_partial;
        if has_partial {
            view.attention = vec![
                "cleanup is pending for a partially removed quarantine; retry work cleanup"
                    .to_string(),
            ];
        }
    }
}

fn rollback_retired(retired: &[RetiredMaterialization]) -> Result<(), WorkViewError> {
    for materialization in retired.iter().rev() {
        #[cfg(unix)]
        restore_tree_modes(&materialization.original_modes)?;
        if !materialization.already_quarantined {
            restore_quarantine(&materialization.original, &materialization.quarantine)?;
        }
    }
    Ok(())
}

fn restore_quarantine(
    original: &std::path::Path,
    quarantine: &std::path::Path,
) -> Result<(), WorkViewError> {
    if original.exists() {
        return Err(WorkViewError::MaterializationChangedAfterReview {
            path: original.display().to_string(),
        });
    }
    fs::rename(quarantine, original)?;
    Ok(())
}

#[cfg(unix)]
fn seal_quarantine(
    quarantine: &std::path::Path,
    display: &str,
) -> Result<Vec<(PathBuf, u32)>, WorkViewError> {
    let mut original_modes = Vec::new();
    if let Err(error) = seal_tree(quarantine, &mut original_modes) {
        restore_tree_modes(&original_modes)?;
        return Err(error);
    }
    match has_open_handles(quarantine) {
        Ok(false) => Ok(original_modes),
        Ok(true) => {
            restore_tree_modes(&original_modes)?;
            Err(WorkViewError::ProjectWriterBusy {
                project_path: display.to_string(),
                reason: "the retired materialization still has open file or directory handles"
                    .to_string(),
            })
        }
        Err(error) => {
            restore_tree_modes(&original_modes)?;
            Err(WorkViewError::ProjectWriterBusy {
                project_path: display.to_string(),
                reason: format!("open-handle verification could not run: {error}"),
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn has_open_handles(path: &std::path::Path) -> std::io::Result<bool> {
    let status = std::process::Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg("+D")
        .arg(path)
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(std::io::Error::other(format!(
            "system lsof exited with {status}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn has_open_handles(path: &std::path::Path) -> std::io::Result<bool> {
    let effective_uid = rustix::process::geteuid().as_raw();
    for process in fs::read_dir("/proc")? {
        let process = match process {
            Ok(process)
                if process
                    .file_name()
                    .to_string_lossy()
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()) =>
            {
                process.path()
            }
            _ => continue,
        };
        // A process whose status we cannot read is not one of ours — `/proc`
        // hides other users' entries under `hidepid`, which hardened hosts and
        // many containers set, and an agent host is often exactly that. We
        // could not inspect its descriptors either way, so skip it rather than
        // abort: propagating here turned "this host hides other users'
        // processes" into "another writer holds the view", which fails every
        // accept on such a host and says something untrue about why.
        let process_uid = match linux_process_uid(&process) {
            Ok(process_uid) => process_uid,
            Err(_) => continue,
        };
        // A process we are refused access to proves nothing: only an OBSERVED
        // handle can show the view is busy. The uid guard below assumes "our
        // own uid means we can read it", which is false whenever we run as
        // root — every system process and kernel thread then matches, and a
        // single denied one aborted the whole scan. That turned an
        // uninspectable neighbour into "another writer holds the view" and
        // failed every accept on such a host, CI included.
        for link in [process.join("cwd"), process.join("root")] {
            match fs::read_link(link) {
                Ok(target) if target.starts_with(path) => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
                Err(error) if process_uid == effective_uid => return Err(error),
                Err(_) => {}
            }
        }
        let descriptors = match fs::read_dir(process.join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) if process_uid == effective_uid => return Err(error),
            Err(_) => continue,
        };
        for descriptor in descriptors.flatten() {
            if fs::read_link(descriptor.path()).is_ok_and(|target| target.starts_with(path)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn linux_process_uid(process: &std::path::Path) -> std::io::Result<u32> {
    let status = match fs::read_to_string(process.join("status")) {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(u32::MAX),
        Err(error) => return Err(error),
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| std::io::Error::other("Linux process status omitted Uid"))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn has_open_handles(_path: &std::path::Path) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "open-handle inspection is not implemented on this Unix platform",
    ))
}

#[cfg(unix)]
fn seal_tree(
    path: &std::path::Path,
    original_modes: &mut Vec<(PathBuf, u32)>,
) -> Result<(), WorkViewError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        let mut children = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for child in children {
            seal_tree(&child, original_modes)?;
        }
    }
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    original_modes.push((path.to_path_buf(), mode));
    let mut permissions = metadata.permissions();
    permissions.set_mode(if metadata.is_dir() { 0o500 } else { 0o400 });
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(unix)]
fn restore_tree_modes(modes: &[(PathBuf, u32)]) -> Result<(), WorkViewError> {
    use std::os::unix::fs::PermissionsExt;

    for (path, mode) in modes.iter().rev() {
        if !path.exists() {
            continue;
        }
        let mut permissions = fs::symlink_metadata(path)?.permissions();
        permissions.set_mode(*mode);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_directories_writable(path: &std::path::Path) -> Result<(), WorkViewError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    for entry in fs::read_dir(path)? {
        make_directories_writable(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn failed_batch_restores_every_earlier_quarantine() {
        let workspace = TempWorkspace::new("work-cleanup-rollback").expect("workspace");
        let mut retired = Vec::new();
        for name in ["first", "middle", "last"] {
            let original = workspace.root().join(name);
            let quarantine = workspace.root().join(format!(".retired-{name}"));
            fs::create_dir(&original).expect("materialization");
            fs::write(original.join("work.txt"), name).expect("work");
            fs::rename(&original, &quarantine).expect("quarantine");
            retired.push(RetiredMaterialization {
                view_id: name.to_string(),
                original,
                quarantine: quarantine.clone(),
                display: name.to_string(),
                already_quarantined: false,
                #[cfg(unix)]
                original_modes: vec![(quarantine.clone(), 0o755)],
            });
        }

        rollback_retired(&retired).expect("rollback");

        for materialization in retired {
            assert!(materialization.original.join("work.txt").is_file());
            assert!(!materialization.quarantine.exists());
        }
    }
}
