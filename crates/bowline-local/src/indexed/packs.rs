use super::*;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackPayload {
    #[serde(default)]
    full_snapshot: bool,
    #[serde(default)]
    documents: Vec<IndexPackDocument>,
    #[serde(default)]
    symbols: Vec<IndexPackSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackDocument {
    path: String,
    body: String,
    #[serde(default)]
    content_id: Option<String>,
    #[serde(default)]
    policy_summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackSymbol {
    path: String,
    name: String,
    kind: String,
    language: String,
    #[serde(default)]
    line_start: u64,
    #[serde(default)]
    line_end: u64,
    #[serde(default)]
    byte_start: u64,
    #[serde(default)]
    byte_end: u64,
}

pub fn import_decrypted_index_pack(
    options: DecryptedIndexPackImport<'_>,
) -> Result<usize, IndexedError> {
    let payload: IndexPackPayload = serde_json::from_slice(options.plaintext)?;
    let mut store = MetadataStore::open(options.db_path)?;
    let policy_root = resolve_project_policy_root(
        Some(options.db_path),
        &options.workspace_id,
        &options.project_id,
    )?;
    store.upsert_index_pack(&IndexPackRecord {
        workspace_id: options.workspace_id.clone(),
        project_id: Some(options.project_id.clone()),
        snapshot_id: Some(options.snapshot_id.clone()),
        object_key: options.object_key,
        byte_len: options.byte_len,
        hash: options.hash,
        state: "ready".to_string(),
        updated_at: options.now.to_string(),
    })?;

    let mut imported = 0_usize;
    let mut imported_paths = BTreeSet::new();
    let full_snapshot = payload.full_snapshot;
    for (ordinal, document) in payload.documents.into_iter().enumerate() {
        let Some(path) = clean_index_pack_path(&document.path) else {
            continue;
        };
        let policy = policy_root
            .as_ref()
            .map(|root| {
                UserPolicy::load_for_path(root, &path).unwrap_or_else(|_| UserPolicy::empty())
            })
            .unwrap_or_else(UserPolicy::empty);
        let decision = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir: false,
                byte_len: Some(document.body.len() as u64),
            },
            &policy,
        );
        if !indexable_for_agents(&decision) {
            let _ = store.purge_index_path(&options.workspace_id, &options.project_id, &path)?;
            continue;
        }
        let source_watermark = ordinal as u64 + 1;
        store.upsert_index_document(&IndexDocumentRecord {
            workspace_id: options.workspace_id.clone(),
            project_id: Some(options.project_id.clone()),
            path: path.clone(),
            snapshot_id: Some(options.snapshot_id.clone()),
            content_id: document.content_id.map(ContentId::new),
            classification: decision.classification,
            mode: decision.mode,
            access: decision.access.clone(),
            policy_summary: document
                .policy_summary
                .unwrap_or_else(|| decision.summary.clone()),
            body_text: document.body.clone(),
            hydration_state: HydrationState::Cold,
            indexed_bytes: document.body.len() as u64,
            source_watermark,
            indexed_watermark: source_watermark,
            state: "ready".to_string(),
            updated_at: options.now.to_string(),
        })?;
        imported_paths.insert(path);
        imported += 1;
    }
    if full_snapshot {
        store.purge_index_paths_for_snapshot_except(
            &options.workspace_id,
            &options.project_id,
            &options.snapshot_id,
            &imported_paths,
        )?;
    }
    let mut symbols_by_path = std::collections::BTreeMap::<String, Vec<SymbolIndexRecord>>::new();
    for (ordinal, symbol) in payload.symbols.into_iter().enumerate() {
        let Some(path) = clean_index_pack_path(&symbol.path) else {
            continue;
        };
        if !imported_paths.contains(&path) {
            continue;
        }
        symbols_by_path
            .entry(path.clone())
            .or_default()
            .push(SymbolIndexRecord {
                id: format!(
                    "sym_pack_{}_{}_{}_{}",
                    options.workspace_id.as_str(),
                    options.project_id.as_str(),
                    stable_hex(path.as_bytes()),
                    ordinal
                ),
                workspace_id: options.workspace_id.clone(),
                project_id: Some(options.project_id.clone()),
                path,
                snapshot_id: Some(options.snapshot_id.clone()),
                name: symbol.name,
                kind: symbol.kind,
                language: symbol.language,
                line_start: symbol.line_start,
                line_end: symbol.line_end,
                byte_start: symbol.byte_start,
                byte_end: symbol.byte_end,
                parser_status: "Ready".to_string(),
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                updated_at: options.now.to_string(),
            });
    }
    for path in imported_paths {
        let records = symbols_by_path.remove(&path).unwrap_or_default();
        store.replace_symbol_records_for_path(
            &options.workspace_id,
            &options.project_id,
            &path,
            records.as_slice(),
        )?;
        store.mark_index_work_ready_under_prefix(
            &options.workspace_id,
            &options.project_id,
            &path,
            options.now,
        )?;
    }
    Ok(imported)
}

fn clean_index_pack_path(path: &str) -> Option<String> {
    let raw = path.replace('\\', "/");
    if raw.starts_with('/') || raw.contains(':') {
        return None;
    }
    let normalized = normalize_workspace_path(&raw);
    if normalized.is_empty()
        || normalized
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return None;
    }
    Some(normalized)
}
