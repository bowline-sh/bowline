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
    ensure_path_in_scope(lease_write_target_path(lease), &path)?;
    Ok(path)
}

pub(super) fn ensure_path_in_scope(lease_root: &str, path: &Path) -> Result<(), AgentError> {
    if path.starts_with(expand_display_path(lease_root)) {
        Ok(())
    } else {
        Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })
    }
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
