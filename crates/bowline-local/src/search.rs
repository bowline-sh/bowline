use std::path::PathBuf;

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, SearchCommandOutput, SearchResult},
    status::WorkspaceStatus,
};

use crate::indexed::{
    IndexedError, IndexedProjectIdentity, build_project_index, build_project_index_with_identity,
    document_for_path, line_match, search_options_with_offset,
};

#[derive(Debug, Clone)]
pub struct SearchCommandOptions {
    pub db_path: Option<PathBuf>,
    pub query: String,
    pub requested_path: Option<String>,
    pub path_prefix: Option<String>,
    pub generated_at: String,
    pub limit: usize,
    pub project_identity: Option<IndexedProjectIdentity>,
}

pub fn search_workspace(
    options: SearchCommandOptions,
) -> Result<SearchCommandOutput, IndexedError> {
    search_workspace_page(options, 0)
}

pub fn search_workspace_page(
    options: SearchCommandOptions,
    offset: usize,
) -> Result<SearchCommandOutput, IndexedError> {
    let project = match options.project_identity {
        Some(identity) => build_project_index_with_identity(
            options.db_path,
            options.requested_path.clone(),
            identity,
            &options.generated_at,
        )?,
        None => build_project_index(
            options.db_path,
            options.requested_path.clone(),
            &options.generated_at,
        )?,
    };
    let mut hits = project
        .text_index
        .search(
            &options.query,
            search_options_with_offset(
                options.path_prefix,
                options.limit.saturating_add(1),
                offset,
            ),
        )
        .into_iter()
        .filter_map(|hit| {
            let document = document_for_path(&project, &hit.path)?;
            let line = line_match(&document.body, &options.query);
            Some(SearchResult {
                path: hit.path,
                score: f64::from(hit.score),
                project_id: Some(project.project_id.clone()),
                snapshot_id: Some(project.snapshot_id.clone()),
                line_start: line.as_ref().map(|line| line.line_start),
                line_end: line.as_ref().map(|line| line.line_end),
                snippet: hit.snippet.or_else(|| line.map(|line| line.snippet)),
                classification: document.classification,
                mode: document.mode,
                access: document.access.clone(),
                hydration_state: document.hydration_state,
            })
        })
        .collect::<Vec<_>>();
    let truncated = hits.len() > options.limit;
    hits.truncate(options.limit);

    Ok(SearchCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Search,
        generated_at: options.generated_at,
        workspace_id: project.workspace_id,
        project_id: project.project_id,
        query: options.query,
        requested_path: Some(project.requested_path),
        index: project.index_status,
        budget: None,
        truncated,
        next_cursor: None,
        results: hits,
        status: WorkspaceStatus::healthy(),
        next_actions: Vec::new(),
    })
}
