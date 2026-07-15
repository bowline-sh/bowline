use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};
#[cfg(test)]
use std::{fs, fs::OpenOptions, io};

#[cfg(test)]
use bowline_core::workspace_graph::FileExecutability;
use bowline_core::{
    ids::SnapshotId,
    workspace_graph::{
        NamespaceEntry, NamespaceEntryKind, WorkspaceRelativePath, normalize_workspace_path,
    },
};
#[cfg(test)]
use bowline_storage::{CacheError, LocalContentCache};

use crate::sync::SnapshotContent;
use crate::sync::paths::{case_fold_path_component, validate_case_folded_prefixes};

use super::{WorkViewError, paths::is_work_view_materialization_eligible};

#[cfg(test)]
use super::{paths::is_owner_only_work_view_policy, safe_materialization::SafeMaterializationRoot};

mod exposure;
mod verified_content;
pub(super) use exposure::{materialize_live_exposure_plan, materialize_snapshot_exposure_plan};
pub(super) use verified_content::materialize_workspace_keyed_content;
use verified_content::{apply_file_permissions, materialize_verified_content};

#[cfg(test)]
use super::paths::apply_owner_only_work_view_permissions;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedFileMethod {
    Clone,
    Copy,
}

#[cfg(test)]
pub fn materialize_snapshot_manifest(
    snapshot: &SnapshotContent,
    project_path: &str,
    cache_root: &Path,
    visible_path: &Path,
) -> Result<Vec<(String, String)>, WorkViewError> {
    let materializable_entries = validate_snapshot_manifest_paths(snapshot, project_path, true)?;
    let manifest = snapshot.manifest();
    let cache = LocalContentCache::open(cache_root).map_err(|error| {
        snapshot_materialization_error(&manifest.snapshot_id, format!("content cache: {error}"))
    })?;
    let staging = SafeMaterializationRoot::new(visible_path)?;
    let mut base_files = Vec::new();
    for (entry, relative_path) in materializable_entries {
        match entry.kind {
            NamespaceEntryKind::Directory => {
                staging.create_dir(&relative_path)?;
            }
            NamespaceEntryKind::File => {
                let content_id = entry.content_id.as_ref().ok_or_else(|| {
                    snapshot_materialization_error(
                        &manifest.snapshot_id,
                        format!("file `{}` is missing content_id", entry.path),
                    )
                })?;
                let destination = staging.prepare_file(&relative_path)?;
                let content_hash = materialize_verified_content(
                    &cache,
                    content_id,
                    &destination,
                    is_owner_only_work_view_policy(entry.classification, entry.mode),
                )
                .map_err(|error| missing_snapshot_bytes(&manifest.snapshot_id, &entry, error))?;
                apply_file_permissions(
                    &staging,
                    &relative_path,
                    entry.executability,
                    is_owner_only_work_view_policy(entry.classification, entry.mode),
                )?;
                base_files.push((
                    normalize_workspace_path(&relative_path.display().to_string()),
                    content_hash,
                ));
            }
            NamespaceEntryKind::Symlink
            | NamespaceEntryKind::Placeholder
            | NamespaceEntryKind::Tombstone => {}
        }
    }
    base_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(base_files)
}

pub fn snapshot_exposed_entries(
    snapshot: &SnapshotContent,
    project_path: &str,
) -> Result<Vec<NamespaceEntry>, WorkViewError> {
    validate_snapshot_manifest_paths(snapshot, project_path, false).map(|entries| {
        entries
            .into_iter()
            .filter(|(entry, _)| {
                matches!(
                    entry.kind,
                    NamespaceEntryKind::Directory | NamespaceEntryKind::File
                )
            })
            .map(|(entry, _)| entry)
            .collect()
    })
}

fn validate_snapshot_manifest_paths(
    snapshot: &SnapshotContent,
    project_path: &str,
    require_eligible: bool,
) -> Result<Vec<(NamespaceEntry, PathBuf)>, WorkViewError> {
    let manifest = snapshot.manifest();
    let entries = super::namespace::collect_prefix(
        snapshot,
        &WorkspaceRelativePath::new(normalize_workspace_path(project_path)),
    )?;
    let mut folded_paths = BTreeMap::<String, String>::new();
    let mut materializable_entries = Vec::new();
    for entry in entries {
        let Some(relative_path) =
            materializable_relative_path(&manifest.snapshot_id, &entry, project_path)?
        else {
            continue;
        };
        if is_bowline_owned_namespace(&relative_path)
            || is_source_control_metadata_path(&relative_path)
            || matches!(
                entry.kind,
                NamespaceEntryKind::Symlink
                    | NamespaceEntryKind::Placeholder
                    | NamespaceEntryKind::Tombstone
            )
            || (require_eligible
                && !is_work_view_materialization_eligible(
                    entry.classification,
                    entry.mode,
                    &entry.access,
                ))
        {
            continue;
        }
        let normalized_entry_path = normalize_workspace_path(&entry.path);
        if normalized_entry_path != entry.path
            || normalized_entry_path.is_empty()
            || normalized_entry_path.starts_with("../")
            || normalized_entry_path.contains("/../")
        {
            return Err(not_relative_safe(&manifest.snapshot_id, &entry.path));
        }
        let normalized_relative = normalize_workspace_path(&relative_path.display().to_string());
        validate_case_folded_prefixes(&normalized_relative, &mut folded_paths).map_err(
            |collision| {
                snapshot_materialization_error(&manifest.snapshot_id, collision.to_string())
            },
        )?;
        materializable_entries.push((entry, relative_path));
    }
    Ok(materializable_entries)
}

#[cfg(test)]
pub fn materialize_base_files_with_methods(
    workspace_root: &Path,
    project_path: &str,
    visible_path: &Path,
) -> Result<Vec<(String, MaterializedFileMethod)>, WorkViewError> {
    let mut methods = Vec::new();
    let source_root = workspace_root.join(normalize_workspace_path(project_path));
    materialize_base_files_inner_with_methods(
        workspace_root,
        &source_root,
        &source_root,
        visible_path,
        &mut methods,
    )?;
    Ok(methods)
}

#[cfg(test)]
fn materialize_base_files_inner_with_methods(
    workspace_root: &Path,
    source_root: &Path,
    path: &Path,
    visible_path: &Path,
    methods: &mut Vec<(String, MaterializedFileMethod)>,
) -> Result<(), WorkViewError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(source_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if is_bowline_owned_namespace(relative) {
            continue;
        }
        if is_source_control_metadata_path(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let workspace_relative = path
            .strip_prefix(workspace_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let workspace_relative_text =
            normalize_workspace_path(&workspace_relative.display().to_string());
        let user_policy =
            crate::policy::UserPolicy::load_for_path(workspace_root, &workspace_relative_text)?;
        let policy = crate::policy::classify_path(
            &crate::policy::PathFacts {
                relative_path: workspace_relative_text,
                is_dir: metadata.is_dir(),
                byte_len: metadata.is_file().then_some(metadata.len()),
            },
            &user_policy,
        );
        if !is_work_view_materialization_eligible(
            policy.classification,
            policy.mode,
            &policy.access,
        ) {
            if metadata.is_dir()
                && user_policy.has_include_below(&normalize_workspace_path(
                    &workspace_relative.display().to_string(),
                ))
            {
                materialize_base_files_inner_with_methods(
                    workspace_root,
                    source_root,
                    &path,
                    visible_path,
                    methods,
                )?;
            }
            continue;
        }
        let destination = visible_path.join(relative);
        if metadata.is_dir() {
            fs::create_dir_all(&destination)?;
            materialize_base_files_inner_with_methods(
                workspace_root,
                source_root,
                &path,
                visible_path,
                methods,
            )?;
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let owner_only = is_owner_only_work_view_policy(policy.classification, policy.mode);
            let method = if owner_only {
                copy_owner_only_file(&path, &destination)?;
                MaterializedFileMethod::Copy
            } else {
                clone_or_copy_file(&path, &destination)?
            };
            apply_owner_only_work_view_permissions(&destination, owner_only)?;
            methods.push((relative.display().to_string(), method));
        }
    }
    Ok(())
}

#[cfg(test)]
fn copy_owner_only_file(source: &Path, destination: &Path) -> io::Result<()> {
    let mut source = fs::File::open(source)?;
    let mut destination = owner_only_destination(destination)?;
    io::copy(&mut source, &mut destination)?;
    Ok(())
}

#[cfg(all(test, unix))]
fn owner_only_destination(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(all(test, not(unix)))]
fn owner_only_destination(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
fn clone_or_copy_file(
    source: &Path,
    destination: &Path,
) -> Result<MaterializedFileMethod, io::Error> {
    if clone_file(source, destination) {
        return Ok(MaterializedFileMethod::Clone);
    }
    let mut source = fs::File::open(source)?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut source, &mut destination)?;
    Ok(MaterializedFileMethod::Copy)
}

#[cfg(test)]
fn clone_file(source: &Path, destination: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let (Some(parent), Some(file_name)) = (destination.parent(), destination.file_name())
        else {
            return false;
        };
        let (Ok(source), Ok(parent)) = (fs::File::open(source), fs::File::open(parent)) else {
            return false;
        };
        rustix::fs::fclonefileat(
            source,
            parent,
            file_name,
            rustix::fs::CloneFlags::NOOWNERCOPY,
        )
        .is_ok()
    }

    #[cfg(target_os = "linux")]
    {
        let (Ok(source), Ok(destination_file)) = (
            fs::File::open(source),
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination),
        ) else {
            return false;
        };
        if rustix::fs::ioctl_ficlone(&destination_file, &source).is_ok() {
            return true;
        }
        drop(destination_file);
        let _ = fs::remove_file(destination);
        false
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        false
    }
}

fn is_bowline_owned_namespace(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(name))
            if name
                .to_str()
                .is_some_and(|name| case_fold_path_component(name) == ".work")
    )
}

fn is_source_control_metadata_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name.to_str().is_some_and(|name| matches!(
                    case_fold_path_component(name).as_str(),
                    ".git" | ".jj" | ".hg" | ".svn"
                ))
        )
    })
}

fn materializable_relative_path(
    snapshot_id: &SnapshotId,
    entry: &NamespaceEntry,
    project_path: &str,
) -> Result<Option<PathBuf>, WorkViewError> {
    let Some(relative_path) = project_relative_path(entry, project_path) else {
        return Ok(None);
    };
    if relative_path.as_os_str().is_empty() {
        if entry.kind == NamespaceEntryKind::Directory {
            return Ok(None);
        }
        return Err(snapshot_materialization_error(
            snapshot_id,
            format!("entry `{}` resolves to the project root", entry.path),
        ));
    }
    ensure_safe_manifest_path(snapshot_id, &relative_path)?;
    Ok(Some(relative_path))
}

fn project_relative_path(entry: &NamespaceEntry, project_path: &str) -> Option<PathBuf> {
    let entry_path = normalize_workspace_path(&entry.path);
    let project_path = normalize_workspace_path(project_path);
    if project_path.is_empty() {
        return Some(PathBuf::from(entry_path));
    }
    if entry_path == project_path {
        return Some(PathBuf::new());
    }
    entry_path
        .strip_prefix(&format!("{project_path}/"))
        .map(PathBuf::from)
}

fn ensure_safe_manifest_path(snapshot_id: &SnapshotId, path: &Path) -> Result<(), WorkViewError> {
    let path_text = path.display().to_string();
    if path_text.split('/').any(|component| component == ".")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(not_relative_safe(snapshot_id, &path_text));
    }
    Ok(())
}

#[cfg(test)]
fn missing_snapshot_bytes(
    snapshot_id: &SnapshotId,
    entry: &NamespaceEntry,
    error: CacheError,
) -> WorkViewError {
    snapshot_materialization_error(
        snapshot_id,
        format!(
            "retained bytes for `{}` are not available locally ({error})",
            entry.path
        ),
    )
}

fn snapshot_materialization_error(snapshot_id: &SnapshotId, reason: String) -> WorkViewError {
    WorkViewError::SnapshotMaterialization {
        snapshot_id: snapshot_id.as_str().to_string(),
        reason,
    }
}

fn not_relative_safe(snapshot_id: &SnapshotId, shown: &str) -> WorkViewError {
    snapshot_materialization_error(
        snapshot_id,
        format!("manifest path `{shown}` is not relative-safe"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bowline_core::{
        ids::{ContentId, PackId, ProjectId, WorkspaceId},
        namespace_snapshot::{NamespaceBuildError, NamespaceReadError},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{
            ContentLayout, ContentLocator, ContentStorage, HydrationState, NamespaceEntry,
            SnapshotDraft, SnapshotKind, workspace_content_id,
        },
    };

    use super::*;

    #[test]
    fn materialization_keeps_env_owner_only_and_skips_local_regenerate_namespaces() {
        let temp = tempfile_dir("bowline-work-view-materialize");
        let root = temp.join("project");
        let visible = temp.join("work");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join(".work/other")).expect("work");
        fs::create_dir_all(root.join(".git")).expect("git");
        fs::create_dir_all(root.join("node_modules/pkg")).expect("dependencies");
        fs::create_dir_all(root.join("target/debug")).expect("build output");
        fs::create_dir_all(root.join(".cache/tool")).expect("cache");
        fs::write(root.join("src/index.ts"), "base").expect("source");
        fs::write(root.join(".env.local"), "SECRET=value").expect("env");
        fs::write(root.join(".git/config"), "[core]").expect("git config");
        fs::write(root.join(".work/other/file"), "internal").expect("internal");
        fs::write(root.join("node_modules/pkg/index.js"), "dependency").expect("dependency");
        fs::write(root.join("target/debug/app"), "build").expect("build");
        fs::write(root.join(".cache/tool/state"), "cache").expect("cache");

        let methods =
            materialize_base_files_with_methods(&temp, "project", &visible).expect("materialize");

        assert_eq!(methods.len(), 2);
        assert!(visible.join("src/index.ts").exists());
        assert_eq!(
            fs::read_to_string(visible.join(".env.local")).expect("materialized env"),
            "SECRET=value"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(visible.join(".env.local"))
                    .expect("env metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!visible.join(".git/config").exists());
        assert!(!visible.join(".work/other/file").exists());
        assert!(!visible.join("node_modules").exists());
        assert!(!visible.join("target").exists());
        assert!(!visible.join(".cache").exists());
    }

    #[test]
    fn live_materialization_honors_user_policy_and_hides_generic_credentials() {
        let temp = tempfile_dir("bowline-work-view-materialize-policy");
        let root = temp.join("project");
        let visible = temp.join("work");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(temp.join(".bowlineignore"), "project/local.txt\n").expect("policy");
        fs::write(root.join("src/index.ts"), "base").expect("source");
        fs::write(root.join("local.txt"), "local only").expect("local");
        fs::write(root.join(".env.local"), "TOKEN=env").expect("env");
        fs::write(root.join("private_key.json"), "credential").expect("credential");
        fs::write(root.join("private.pem"), "private key").expect("key");

        let methods =
            materialize_base_files_with_methods(&temp, "project", &visible).expect("materialize");

        assert_eq!(methods.len(), 2);
        assert!(visible.join("src/index.ts").exists());
        assert!(visible.join(".env.local").exists());
        assert!(!visible.join("local.txt").exists());
        assert!(!visible.join("private_key.json").exists());
        assert!(!visible.join("private.pem").exists());
    }

    #[test]
    fn snapshot_manifest_case_collision_fails_before_writing() {
        let temp = tempfile_dir("bowline-work-view-manifest-collision");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let upper = retained_content(&cache, [1_u8; 32], b"upper");
        let lower = retained_content(&cache, [2_u8; 32], b"lower");
        let manifest = manifest(vec![
            manifest_file("apps/web/src/App.ts", upper, b"upper".len() as u64),
            manifest_file("apps/web/src/app.ts", lower, b"lower".len() as u64),
        ]);
        let expected_snapshot_id = manifest.manifest().snapshot_id.as_str().to_string();

        let error = materialize_snapshot_manifest(&manifest, "apps/web", &cache_root, &visible)
            .expect_err("case collision should fail");

        match error {
            WorkViewError::SnapshotMaterialization {
                snapshot_id,
                reason,
            } => {
                assert_eq!(snapshot_id, expected_snapshot_id);
                assert!(reason.contains("src/App.ts"));
                assert!(reason.contains("src/app.ts"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(!visible.join("src/App.ts").exists());
        assert!(!visible.join("src/app.ts").exists());
    }

    #[test]
    fn snapshot_manifest_ignores_skipped_entries_for_case_collision_preflight() {
        let temp = tempfile_dir("bowline-work-view-manifest-skipped-collision");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let lower = retained_content(&cache, [2_u8; 32], b"lower");
        let mut tombstone =
            manifest_file("apps/web/src/App.ts", lower.clone(), b"lower".len() as u64);
        tombstone.kind = NamespaceEntryKind::Tombstone;
        tombstone.content_id = None;
        tombstone.content_layout = None;
        tombstone.byte_len = None;
        let manifest = manifest(vec![
            tombstone,
            manifest_file("apps/web/src/app.ts", lower, b"lower".len() as u64),
        ]);

        let base_files =
            materialize_snapshot_manifest(&manifest, "apps/web", &cache_root, &visible)
                .expect("materialize");

        assert_eq!(
            base_files,
            vec![(
                "src/app.ts".to_string(),
                format!("b3_{}", blake3::hash(b"lower").to_hex())
            )]
        );
        assert_eq!(
            fs::read(visible.join("src/app.ts")).expect("lower materialized"),
            b"lower"
        );
    }

    #[test]
    fn snapshot_materialization_prefix_is_component_correct() {
        let temp = tempfile_dir("bowline-work-view-component-prefix");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let selected = retained_content(&cache, [9_u8; 32], b"selected");
        let sibling = retained_content(&cache, [10_u8; 32], b"sibling");
        let snapshot = manifest(vec![
            manifest_file("apps/web/src/index.ts", selected, b"selected".len() as u64),
            manifest_file("apps/webish/src/index.ts", sibling, b"sibling".len() as u64),
        ]);

        materialize_snapshot_manifest(&snapshot, "apps/web", &cache_root, &visible)
            .expect("materialize selected component");

        assert_eq!(
            fs::read(visible.join("src/index.ts")).expect("selected file"),
            b"selected"
        );
        assert!(!visible.join("../webish/src/index.ts").exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_manifest_materializes_executable_files_runnable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile_dir("bowline-work-view-manifest-executable");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let content = retained_content(&cache, [7_u8; 32], b"#!/bin/sh\n");
        let mut entry = manifest_file("apps/web/bin/dev", content, b"#!/bin/sh\n".len() as u64);
        entry.executability = FileExecutability::Executable;
        let manifest = manifest(vec![entry]);

        materialize_snapshot_manifest(&manifest, "apps/web", &cache_root, &visible)
            .expect("materialize");

        assert_eq!(
            fs::metadata(visible.join("bin/dev"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn snapshot_manifest_rejects_current_dir_segments_before_writing() {
        let temp = tempfile_dir("bowline-work-view-manifest-dot-collision");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let upper = retained_content(&cache, [1_u8; 32], b"upper");
        let lower = retained_content(&cache, [2_u8; 32], b"lower");
        let error = manifest_result(vec![
            manifest_file("apps/web/src/./App.ts", upper, b"upper".len() as u64),
            manifest_file("apps/web/src/app.ts", lower, b"lower".len() as u64),
        ])
        .expect_err("current-dir segment should fail");

        assert!(matches!(
            error,
            NamespaceBuildError::Read(NamespaceReadError::InvalidPath { .. })
        ));
        assert!(!visible.join("src/app.ts").exists());
    }

    #[test]
    fn snapshot_manifest_rejects_current_dir_private_namespace_bypass() {
        let temp = tempfile_dir("bowline-work-view-manifest-dot-private");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let work = retained_content(&cache, [6_u8; 32], b"internal");
        let error = manifest_result(vec![manifest_file(
            "apps/web/./.work/x",
            work,
            b"internal".len() as u64,
        )])
        .expect_err("current-dir private namespace should fail");

        assert!(matches!(
            error,
            NamespaceBuildError::Read(NamespaceReadError::InvalidPath { .. })
        ));
        assert!(!visible.join(".work/x").exists());
    }

    #[test]
    fn snapshot_manifest_skips_case_variant_private_namespaces() {
        let temp = tempfile_dir("bowline-work-view-manifest-private-case");
        let cache_root = temp.join("cache");
        let visible = temp.join("work");
        let cache = LocalContentCache::open(&cache_root).expect("cache");
        let source = retained_content(&cache, [3_u8; 32], b"source");
        let git = retained_content(&cache, [4_u8; 32], b"[core]\n");
        let env = retained_content(&cache, [5_u8; 32], b"TOKEN=secret");
        let work = retained_content(&cache, [6_u8; 32], b"internal");
        let credential = retained_content(&cache, [7_u8; 32], b"credential");
        let hidden = retained_content(&cache, [8_u8; 32], b"hidden");
        let mut env_entry = manifest_file("apps/web/.ENV.local", env, b"TOKEN=secret".len() as u64);
        env_entry.classification = PathClassification::ProjectEnv;
        env_entry.mode = MaterializationMode::ProjectEnv;
        let mut credential_entry = manifest_file(
            "apps/web/private_key.json",
            credential,
            b"credential".len() as u64,
        );
        credential_entry.classification = PathClassification::SecretLooking;
        credential_entry.mode = MaterializationMode::EncryptedSync;
        credential_entry.access = vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden];
        let mut hidden_entry =
            manifest_file("apps/web/agent-hidden.txt", hidden, b"hidden".len() as u64);
        hidden_entry.access = vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden];
        let manifest = manifest(vec![
            manifest_file("apps/web/src/index.ts", source, b"source".len() as u64),
            manifest_file("apps/web/.GIT/config", git, b"[core]\n".len() as u64),
            env_entry,
            credential_entry,
            hidden_entry,
            manifest_file("apps/web/.Work/x", work, b"internal".len() as u64),
        ]);

        let base_files =
            materialize_snapshot_manifest(&manifest, "apps/web", &cache_root, &visible)
                .expect("materialize");

        assert_eq!(base_files.len(), 2);
        assert!(base_files.iter().any(|(path, _)| path == "src/index.ts"));
        assert!(base_files.iter().any(|(path, _)| path == ".ENV.local"));
        assert_eq!(
            fs::read(visible.join("src/index.ts")).expect("source materialized"),
            b"source"
        );
        assert!(!visible.join(".GIT/config").exists());
        assert_eq!(
            fs::read(visible.join(".ENV.local")).expect("env materialized"),
            b"TOKEN=secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(visible.join(".ENV.local"))
                    .expect("env metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!visible.join(".Work/x").exists());
        assert!(!visible.join("private_key.json").exists());
        assert!(!visible.join("agent-hidden.txt").exists());
    }

    #[test]
    fn root_project_manifest_paths_are_already_relative() {
        let entry = NamespaceEntry {
            path: "src/index.ts".to_string(),
            kind: NamespaceEntryKind::File,
            classification: bowline_core::policy::PathClassification::WorkspaceSync,
            mode: bowline_core::policy::MaterializationMode::WorkspaceSync,
            access: Vec::new(),
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: bowline_core::workspace_graph::HydrationState::Local,
        };

        assert_eq!(
            project_relative_path(&entry, ""),
            Some(PathBuf::from("src/index.ts"))
        );
    }

    fn manifest(entries: Vec<NamespaceEntry>) -> SnapshotContent {
        manifest_result(entries).expect("page-backed snapshot")
    }

    fn manifest_result(
        entries: Vec<NamespaceEntry>,
    ) -> Result<SnapshotContent, NamespaceBuildError> {
        let workspace_id = WorkspaceId::new("ws_materialize");
        let snapshot_id =
            crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        SnapshotContent::new(
            SnapshotDraft {
                schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
                snapshot_id,
                workspace_id,
                project_id: Some(ProjectId::new("proj_web")),
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries,
                refs: Vec::new(),
            },
            BTreeMap::new(),
            [7; 32],
        )
    }

    fn retained_content(cache: &LocalContentCache, digest: [u8; 32], bytes: &[u8]) -> ContentId {
        let content_id = workspace_content_id(digest, bytes);
        cache.put_content(&content_id, bytes).expect("put content");
        cache
            .get_content(&content_id, digest)
            .expect("verified content");
        content_id
    }

    fn manifest_file(path: &str, content_id: ContentId, byte_len: u64) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: Some(content_id.clone()),
            content_layout: Some(
                ContentLayout::single_segment(ContentLocator {
                    content_id,
                    storage: ContentStorage::Packed,
                    raw_size: byte_len,
                    pack_id: Some(PackId::new("pk_materialize")),
                    offset: Some(0),
                    length: Some(byte_len),
                })
                .expect("test layout"),
            ),
            symlink_target: None,
            byte_len: Some(byte_len),
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            blake3::hash(name.as_bytes()).to_hex()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
