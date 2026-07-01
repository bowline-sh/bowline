use super::*;

#[derive(Debug, Clone)]
pub(super) struct ResolvedScope {
    pub(super) workspace_id: Option<WorkspaceId>,
    pub(super) project_id: Option<ProjectId>,
    pub(super) project_path: Option<String>,
}

impl ResolvedScope {
    pub(super) fn event_query(&self, limit: u32) -> EventQuery {
        EventQuery {
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            path_prefix: self.project_path.clone(),
            limit,
        }
    }
}

pub(super) fn resolve_scope(
    store: &MetadataStore,
    requested_path: Option<&str>,
    workspace_scope: bool,
) -> Result<ResolvedScope, LocalStatusError> {
    let workspace_id = store.current_workspace()?.map(|record| record.id);
    let project = if workspace_scope {
        None
    } else if let Some(path) = requested_path {
        store.current_project_by_path(path)?
    } else {
        None
    };

    Ok(ResolvedScope {
        workspace_id,
        project_id: project.as_ref().map(|record| record.id.clone()),
        project_path: project.map(|record| record.path),
    })
}
