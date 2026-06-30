use std::path::PathBuf;

use bowline_core::{
    commands::{
        CONTRACT_VERSION, CommandName, SymbolCommandOutput, SymbolKind, SymbolLanguage,
        SymbolResult,
    },
    status::WorkspaceStatus,
};

use crate::indexed::{
    IndexedError, IndexedProjectIdentity, build_project_index, build_project_index_with_identity,
    document_for_path, symbol_options_with_offset,
};

#[derive(Debug, Clone)]
pub struct SymbolCommandOptions {
    pub db_path: Option<PathBuf>,
    pub query: String,
    pub requested_path: Option<String>,
    pub path_prefix: Option<String>,
    pub generated_at: String,
    pub limit: usize,
    pub project_identity: Option<IndexedProjectIdentity>,
}

pub fn lookup_symbols(options: SymbolCommandOptions) -> Result<SymbolCommandOutput, IndexedError> {
    lookup_symbols_page(options, 0)
}

pub fn lookup_symbols_page(
    options: SymbolCommandOptions,
    offset: usize,
) -> Result<SymbolCommandOutput, IndexedError> {
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
    let mut symbols = project
        .symbol_index
        .lookup(
            &options.query,
            symbol_options_with_offset(
                options.path_prefix,
                options.limit.saturating_add(1),
                offset,
            ),
        )
        .into_iter()
        .filter_map(|record| {
            let document = document_for_path(&project, &record.path)?;
            Some(SymbolResult {
                name: record.name,
                kind: map_symbol_kind(record.kind, record.language),
                language: map_language(record.language),
                path: record.path,
                line_start: record.line_range.start as u64,
                line_end: record.line_range.end.saturating_sub(1) as u64,
                project_id: Some(project.project_id.clone()),
                snapshot_id: Some(project.snapshot_id.clone()),
                container: None,
                signature: None,
                reference_count: None,
                classification: document.classification,
                access: document.access.clone(),
                hydration_state: document.hydration_state,
            })
        })
        .collect::<Vec<_>>();
    let truncated = symbols.len() > options.limit;
    symbols.truncate(options.limit);

    Ok(SymbolCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Symbols,
        generated_at: options.generated_at,
        workspace_id: project.workspace_id,
        project_id: project.project_id,
        query: options.query,
        requested_path: Some(project.requested_path),
        index: project.index_status,
        budget: None,
        truncated,
        next_cursor: None,
        symbols,
        status: WorkspaceStatus::healthy(),
        next_actions: Vec::new(),
    })
}

fn map_symbol_kind(
    kind: bowline_index::SymbolKind,
    _language: bowline_index::Language,
) -> SymbolKind {
    match kind {
        bowline_index::SymbolKind::Function => SymbolKind::Function,
        bowline_index::SymbolKind::Class => SymbolKind::Class,
        bowline_index::SymbolKind::Interface => SymbolKind::Interface,
        bowline_index::SymbolKind::Variable => SymbolKind::Variable,
        bowline_index::SymbolKind::Struct => SymbolKind::Struct,
        bowline_index::SymbolKind::Enum => SymbolKind::Enum,
        bowline_index::SymbolKind::Trait => SymbolKind::Trait,
        bowline_index::SymbolKind::Type => SymbolKind::Type,
        bowline_index::SymbolKind::Import => SymbolKind::Import,
        bowline_index::SymbolKind::Export => SymbolKind::Export,
    }
}

fn map_language(language: bowline_index::Language) -> SymbolLanguage {
    match language {
        bowline_index::Language::TypeScript => SymbolLanguage::TypeScript,
        bowline_index::Language::JavaScript => SymbolLanguage::JavaScript,
        bowline_index::Language::Python => SymbolLanguage::Python,
        bowline_index::Language::Rust => SymbolLanguage::Rust,
        bowline_index::Language::Go => SymbolLanguage::Go,
    }
}
