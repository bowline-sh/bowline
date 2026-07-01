use super::*;

pub(super) fn hydration_target(
    store: &MetadataStore,
    lease: &AgentLease,
    path: &Path,
    requested_content_id: Option<&str>,
) -> Result<(u64, Option<ContentId>), AgentError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let metadata_path = lease_project_metadata_path(store, lease, path)?;
            let projected = store.projected_node_by_path(&lease.workspace_id, &metadata_path)?;
            if let Some(projected) = projected {
                if projected.kind != NamespaceEntryKind::File {
                    return Err(AgentError::ToolDenied {
                        code: "hydration-requires-file".to_string(),
                    });
                }
                if let Some(requested) = requested_content_id {
                    let Some(projected_content_id) = projected.content_id.as_ref() else {
                        return Err(AgentError::ToolDenied {
                            code: "content-id-mismatch".to_string(),
                        });
                    };
                    if projected_content_id.as_str() != requested {
                        return Err(AgentError::ToolDenied {
                            code: "content-id-mismatch".to_string(),
                        });
                    }
                }
                return Ok((metadata.len(), projected.content_id));
            }
            if requested_content_id.is_some() {
                return Err(AgentError::ToolDenied {
                    code: "content-id-unverified".to_string(),
                });
            }
            return Ok((metadata.len(), None));
        }
        Ok(_) => {
            return Err(AgentError::ToolDenied {
                code: "hydration-requires-file".to_string(),
            });
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }

    let metadata_path = lease_project_metadata_path(store, lease, path)?;
    let projected = store
        .projected_node_by_path(&lease.workspace_id, &metadata_path)?
        .ok_or_else(|| AgentError::ToolDenied {
            code: "missing-file".to_string(),
        })?;
    if projected.kind != NamespaceEntryKind::File {
        return Err(AgentError::ToolDenied {
            code: "hydration-requires-file".to_string(),
        });
    }
    let content_id = projected.content_id.ok_or_else(|| AgentError::ToolDenied {
        code: "hydration-size-unknown".to_string(),
    })?;
    if requested_content_id.is_some_and(|requested| content_id.as_str() != requested) {
        return Err(AgentError::ToolDenied {
            code: "content-id-mismatch".to_string(),
        });
    }
    let locator = store
        .content_locator(&lease.workspace_id, &content_id)?
        .ok_or_else(|| AgentError::ToolDenied {
            code: "hydration-size-unknown".to_string(),
        })?;
    Ok((locator.locator.raw_size, Some(content_id)))
}

pub(super) fn lease_index_identity(
    lease: &AgentLease,
    policy_path_prefix: Option<String>,
    max_scan_files: usize,
) -> IndexedProjectIdentity {
    IndexedProjectIdentity {
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        snapshot_id: Some(lease.base_snapshot_id.clone()),
        policy_path_prefix,
        max_scan_files: Some(max_scan_files),
    }
}

pub(super) fn lease_project_metadata_path(
    store: &MetadataStore,
    lease: &AgentLease,
    path: &Path,
) -> Result<String, AgentError> {
    let relative = lease_relative_filter(lease, path).ok_or_else(|| AgentError::ToolDenied {
        code: "path-outside-lease".to_string(),
    })?;
    let project = store
        .project_by_id(&lease.workspace_id, &lease.project_id)?
        .ok_or_else(|| AgentError::MissingProject {
            path: lease.project_id.as_str().to_string(),
        })?;
    Ok(format!(
        "{}/{}",
        normalize_workspace_path(&project.path).trim_end_matches('/'),
        normalize_workspace_path(&relative).trim_start_matches('/')
    ))
}

pub(super) fn hydration_queue_content_matches(
    existing: Option<&ContentId>,
    requested: Option<&ContentId>,
) -> bool {
    match (existing, requested) {
        (Some(existing), Some(requested)) => existing == requested,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

pub(super) fn lease_relative_filter(lease: &AgentLease, path: &Path) -> Option<String> {
    let root = expand_display_path(lease_write_target_path(lease));
    let relative = path.strip_prefix(root).ok()?;
    let relative = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    (!relative.is_empty()).then_some(relative)
}
