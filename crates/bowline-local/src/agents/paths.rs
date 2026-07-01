use super::*;

pub(super) fn resolve_db_path(db_path: Option<PathBuf>) -> Result<PathBuf, AgentError> {
    db_path
        .map(Ok)
        .unwrap_or_else(default_database_path)
        .map_err(Into::into)
}

pub(super) fn scoped_path(root: &str, requested: &str) -> Result<PathBuf, AgentError> {
    let root = expand_display_path(root);
    let mut candidate = root.clone();
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => candidate.push(part),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
        }
    }
    if candidate.starts_with(&root) {
        ensure_no_symlink_components(&root, &candidate)?;
        Ok(candidate)
    } else {
        Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })
    }
}

pub(super) fn scoped_read_path(lease: &AgentLease, requested: &str) -> Result<PathBuf, AgentError> {
    let path = scoped_path(lease_write_target_path(lease), requested)?;
    ensure_path_in_scope(
        lease_write_target_path(lease),
        &path,
        &lease.scopes.read.roots,
    )?;
    Ok(path)
}

pub(super) fn scoped_write_path(
    lease: &AgentLease,
    requested: &str,
) -> Result<PathBuf, AgentError> {
    let path = scoped_path(lease_write_target_path(lease), requested)?;
    ensure_path_in_scope(
        lease_write_target_path(lease),
        &path,
        &lease.scopes.write.roots,
    )?;
    Ok(path)
}

pub(super) fn ensure_path_in_scope(
    lease_root: &str,
    path: &Path,
    scope_roots: &[String],
) -> Result<(), AgentError> {
    if scope_roots
        .iter()
        .map(|root| scope_root_path(lease_root, root))
        .any(|root| path.starts_with(root))
    {
        Ok(())
    } else {
        Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })
    }
}

pub(super) fn scope_root_path(lease_root: &str, root: &str) -> PathBuf {
    let expanded = expand_display_path(root);
    if expanded.is_absolute() || root == "~" || root.starts_with("~/") {
        expanded
    } else {
        expand_display_path(lease_root).join(expanded)
    }
}

pub(super) fn agent_read_allowed(lease_root: &str, path: &Path) -> bool {
    let Some(decision) = agent_path_decision(lease_root, path) else {
        return false;
    };
    !matches!(
        decision.classification,
        PathClassification::ProjectEnv
            | PathClassification::SecretLooking
            | PathClassification::LocalOnly
            | PathClassification::Blocked
    ) && decision.access.contains(&AccessFlag::AgentReadable)
        && !decision.access.contains(&AccessFlag::AgentHidden)
}

pub(super) fn agent_write_allowed_decision(decision: &crate::policy::PathPolicyDecision) -> bool {
    matches!(
        decision.classification,
        PathClassification::WorkspaceSync | PathClassification::LargeFile
    ) && !decision.access.contains(&AccessFlag::AgentHidden)
}

pub(super) fn agent_path_decision(
    lease_root: &str,
    path: &Path,
) -> Option<crate::policy::PathPolicyDecision> {
    let root = expand_display_path(lease_root);
    let relative = path.strip_prefix(&root).ok()?;
    let relative_path = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let metadata = fs::symlink_metadata(path).ok();
    let policy =
        UserPolicy::load_for_path(&root, &relative_path).unwrap_or_else(|_| UserPolicy::empty());
    Some(classify_path(
        &PathFacts {
            relative_path,
            is_dir: metadata.as_ref().is_some_and(|metadata| metadata.is_dir()),
            byte_len: metadata.as_ref().map(|metadata| metadata.len()),
        },
        &policy,
    ))
}

pub(super) fn ensure_no_symlink_components(root: &Path, path: &Path) -> Result<(), AgentError> {
    if fs::symlink_metadata(root)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        });
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn create_parent_dirs_without_symlinks(
    root: &Path,
    parent: &Path,
) -> Result<(), AgentError> {
    if fs::symlink_metadata(root)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        });
    }
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => current.push(part),
            Component::CurDir => continue,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path component is not a directory",
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn lease_work_view_name(task: &str, generated_at: &str) -> String {
    let words = task
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .take(3)
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let prefix = if words.is_empty() {
        "agent".to_string()
    } else {
        words.join("-")
    };
    format!("agent-{}-{}", prefix, stable_token(generated_at))
}

pub(super) fn agent_work_view_id(workspace_id: &str, project_id: &str, name: &str) -> WorkViewId {
    let input = format!("{workspace_id}:{project_id}:{name}");
    WorkViewId::new(format!(
        "work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

pub(super) fn display_path_for_agent_work_view(
    root: &str,
    project_path: &str,
    name: &str,
) -> String {
    let path = expand_display_path(root)
        .join(".work")
        .join(normalize_workspace_path(project_path))
        .join(name);
    display_path(&path)
}

pub(super) fn display_path_for_project(root: &str, project_path: &str) -> String {
    let normalized = normalize_workspace_path(project_path);
    let root = expand_display_path(root);
    let path = if normalized.is_empty() {
        root
    } else {
        root.join(normalized)
    };
    display_path(&path)
}

pub(super) fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

pub(super) fn stable_token(value: &str) -> String {
    let hash = blake3::hash(value.as_bytes()).to_hex().to_string();
    hash.chars().take(12).collect()
}

pub(super) fn redacted_task_label(task: &str) -> String {
    let redacted = crate::setup::redact_setup_text(task).text;
    let task = redacted.trim();
    if task.is_empty() {
        "agent task".to_string()
    } else {
        task.to_string()
    }
}

pub(super) fn json_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other);
            map
        }
    }
}
