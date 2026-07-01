use super::*;

pub(super) fn access_flags_for_row(access: &[AccessFlag]) -> AccessFlags {
    AccessFlags {
        policy_readable: access.contains(&AccessFlag::HumanReadable)
            || access.contains(&AccessFlag::AgentReadable),
        lease_readable: access.contains(&AccessFlag::AgentReadable),
        generated: false,
        local_only: false,
    }
}

pub(super) fn access_summary(access: &[AccessFlag]) -> String {
    if access.is_empty() {
        return "policy:unknown".to_string();
    }
    access
        .iter()
        .map(|flag| format!("{flag:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn symbol_record_to_store(
    project: &IndexedProject,
    document: &IndexedDocument,
    record: SymbolRecord,
    index: usize,
    now: &str,
) -> SymbolIndexRecord {
    SymbolIndexRecord {
        id: format!(
            "sym_{}_{}_{}_{}_{}",
            project.workspace_id.as_str(),
            project.project_id.as_str(),
            stable_hex(document.path.as_bytes()),
            stable_hex(record.name.as_bytes()),
            index
        ),
        workspace_id: project.workspace_id.clone(),
        project_id: Some(project.project_id.clone()),
        path: document.path.clone(),
        snapshot_id: Some(project.snapshot_id.clone()),
        name: record.name,
        kind: format!("{:?}", record.kind),
        language: format!("{:?}", record.language),
        line_start: record.line_range.start as u64,
        line_end: record.line_range.end as u64,
        byte_start: record.byte_range.start as u64,
        byte_end: record.byte_range.end as u64,
        parser_status: format!("{:?}", record.parser_status),
        access: document.access.clone(),
        updated_at: now.to_string(),
    }
}

pub(super) fn symbol_record_from_store(record: &SymbolIndexRecord) -> Option<SymbolRecord> {
    Some(SymbolRecord {
        name: record.name.clone(),
        kind: parse_symbol_kind(&record.kind)?,
        language: parse_language(&record.language)?,
        path: record.path.clone(),
        project_id: record.project_id.as_ref()?.as_str().to_string(),
        snapshot_id: record.snapshot_id.as_ref()?.as_str().to_string(),
        byte_range: record.byte_start as usize..record.byte_end as usize,
        line_range: record.line_start as usize..record.line_end as usize,
        parser_status: parse_index_readiness(&record.parser_status)?,
        access: access_flags_for_row(&record.access),
    })
}

pub(super) fn parse_symbol_kind(value: &str) -> Option<bowline_index::SymbolKind> {
    match value {
        "Function" => Some(bowline_index::SymbolKind::Function),
        "Class" => Some(bowline_index::SymbolKind::Class),
        "Interface" => Some(bowline_index::SymbolKind::Interface),
        "Variable" => Some(bowline_index::SymbolKind::Variable),
        "Struct" => Some(bowline_index::SymbolKind::Struct),
        "Enum" => Some(bowline_index::SymbolKind::Enum),
        "Trait" => Some(bowline_index::SymbolKind::Trait),
        "Type" => Some(bowline_index::SymbolKind::Type),
        "Import" => Some(bowline_index::SymbolKind::Import),
        "Export" => Some(bowline_index::SymbolKind::Export),
        _ => None,
    }
}

pub(super) fn parse_language(value: &str) -> Option<bowline_index::Language> {
    match value {
        "TypeScript" => Some(bowline_index::Language::TypeScript),
        "JavaScript" => Some(bowline_index::Language::JavaScript),
        "Python" => Some(bowline_index::Language::Python),
        "Rust" => Some(bowline_index::Language::Rust),
        "Go" => Some(bowline_index::Language::Go),
        _ => None,
    }
}

pub(super) fn parse_index_readiness(value: &str) -> Option<bowline_index::IndexReadiness> {
    match value {
        "Ready" => Some(bowline_index::IndexReadiness::Ready),
        "Stale" => Some(bowline_index::IndexReadiness::Stale),
        "Rebuilding" => Some(bowline_index::IndexReadiness::Rebuilding),
        "Degraded" => Some(bowline_index::IndexReadiness::Degraded),
        _ => None,
    }
}

pub fn language_for_path(path: &str) -> Option<bowline_index::Language> {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("ts" | "tsx") => Some(bowline_index::Language::TypeScript),
        Some("js" | "jsx" | "mjs" | "cjs") => Some(bowline_index::Language::JavaScript),
        Some("py") => Some(bowline_index::Language::Python),
        Some("rs") => Some(bowline_index::Language::Rust),
        Some("go") => Some(bowline_index::Language::Go),
        _ => None,
    }
}

pub(super) fn manifest_symbol_records(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match file_name {
        "package.json" => package_json_symbols(path, project_id, snapshot_id, body, access),
        "Cargo.toml" => cargo_toml_symbols(path, project_id, snapshot_id, body, access),
        "pyproject.toml" => pyproject_symbols(path, project_id, snapshot_id, body, access),
        "go.mod" => go_mod_symbols(path, project_id, snapshot_id, body, access),
        _ => Vec::new(),
    }
}

pub(super) fn package_json_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value.get("name").and_then(|value| value.as_str()) {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::JavaScript,
            body,
            access,
        ));
    }
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(object) = value.get(section).and_then(|value| value.as_object()) {
            for name in object.keys() {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::JavaScript,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

pub(super) fn cargo_toml_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = toml::from_str::<toml::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
    {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::Rust,
            body,
            access,
        ));
    }
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = value.get(section).and_then(|value| value.as_table()) {
            for name in table.keys() {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Rust,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

pub(super) fn pyproject_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = toml::from_str::<toml::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value
        .get("project")
        .and_then(|project| project.get("name"))
        .and_then(|name| name.as_str())
    {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::Python,
            body,
            access,
        ));
    }
    if let Some(dependencies) = value
        .get("project")
        .and_then(|project| project.get("dependencies"))
        .and_then(|dependencies| dependencies.as_array())
    {
        for dependency in dependencies.iter().filter_map(|value| value.as_str()) {
            if let Some(name) = package_name_from_requirement(dependency) {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    &name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Python,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

pub(super) fn go_mod_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let mut records = Vec::new();
    let mut in_require_block = false;
    for line in body.lines().map(str::trim) {
        if line.starts_with("//") || line.is_empty() {
            continue;
        }
        if in_require_block {
            if line == ")" {
                in_require_block = false;
                continue;
            }
            if let Some(module) = first_requirement_token(line) {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    module,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Go,
                    body,
                    access,
                ));
            }
            continue;
        }
        if let Some(module) = line
            .strip_prefix("module ")
            .and_then(first_requirement_token)
        {
            records.push(manifest_symbol(
                path,
                project_id,
                snapshot_id,
                module,
                bowline_index::SymbolKind::Export,
                bowline_index::Language::Go,
                body,
                access,
            ));
        } else if line == "require (" {
            in_require_block = true;
        } else if let Some(module) = line
            .strip_prefix("require ")
            .and_then(first_requirement_token)
        {
            records.push(manifest_symbol(
                path,
                project_id,
                snapshot_id,
                module,
                bowline_index::SymbolKind::Import,
                bowline_index::Language::Go,
                body,
                access,
            ));
        }
    }
    records
}

#[allow(clippy::too_many_arguments)]
pub(super) fn manifest_symbol(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    name: impl Into<String>,
    kind: bowline_index::SymbolKind,
    language: bowline_index::Language,
    body: &str,
    access: AccessFlags,
) -> SymbolRecord {
    SymbolRecord {
        name: name.into(),
        kind,
        language,
        path: path.to_string(),
        project_id: project_id.as_str().to_string(),
        snapshot_id: snapshot_id.as_str().to_string(),
        byte_range: 0..body.len(),
        line_range: 1..2,
        parser_status: bowline_index::IndexReadiness::Ready,
        access,
    }
}

pub(super) fn package_name_from_requirement(requirement: &str) -> Option<String> {
    first_requirement_token(requirement)
        .map(|token| {
            token
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                .collect()
        })
        .filter(|name: &String| !name.is_empty())
}

pub(super) fn first_requirement_token(input: &str) -> Option<&str> {
    input
        .split_whitespace()
        .next()
        .filter(|token| !token.is_empty())
}

pub(super) fn stable_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    hash.iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
