use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use tantivy::{
    Index,
    collector::TopDocs,
    doc,
    query::QueryParser,
    schema::{STORED, STRING, Schema, TEXT, Value},
};
use tree_sitter::Parser;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Namespace,
    Text,
    Symbols,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexReadiness {
    Ready,
    Stale,
    Rebuilding,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DegradedReason {
    SourceWatermarkAhead,
    Rebuilding,
    ParserFailed,
    PolicyHidden,
    ColdContentNeedsHydration,
    UnsupportedLanguage,
    CorruptDerivedState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexFreshness {
    pub kind: IndexKind,
    pub readiness: IndexReadiness,
    pub source_watermark: u64,
    pub indexed_watermark: u64,
    pub reason: Option<DegradedReason>,
    pub updated_at: String,
}

impl IndexFreshness {
    pub fn from_watermarks(
        kind: IndexKind,
        source_watermark: u64,
        indexed_watermark: u64,
        updated_at: impl Into<String>,
    ) -> Self {
        let stale = source_watermark > indexed_watermark;
        Self {
            kind,
            readiness: if stale {
                IndexReadiness::Stale
            } else {
                IndexReadiness::Ready
            },
            source_watermark,
            indexed_watermark,
            reason: stale.then_some(DegradedReason::SourceWatermarkAhead),
            updated_at: updated_at.into(),
        }
    }

    pub fn rebuilding(
        kind: IndexKind,
        source_watermark: u64,
        updated_at: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            readiness: IndexReadiness::Rebuilding,
            source_watermark,
            indexed_watermark: 0,
            reason: Some(DegradedReason::Rebuilding),
            updated_at: updated_at.into(),
        }
    }

    pub fn degraded(
        kind: IndexKind,
        source_watermark: u64,
        indexed_watermark: u64,
        reason: DegradedReason,
        updated_at: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            readiness: IndexReadiness::Degraded,
            source_watermark,
            indexed_watermark,
            reason: Some(reason),
            updated_at: updated_at.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathClassification {
    Source,
    Text,
    Config,
    Secret,
    Generated,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageMode {
    Local,
    Synced,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HydrationState {
    Hydrated,
    Partial,
    Cold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessFlags {
    pub policy_readable: bool,
    pub lease_readable: bool,
    pub generated: bool,
    pub local_only: bool,
}

impl AccessFlags {
    pub fn readable() -> Self {
        Self {
            policy_readable: true,
            lease_readable: true,
            generated: false,
            local_only: false,
        }
    }

    pub fn hidden() -> Self {
        Self {
            policy_readable: false,
            lease_readable: false,
            generated: false,
            local_only: false,
        }
    }

    fn can_return(self) -> bool {
        self.policy_readable && self.lease_readable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceEntry {
    pub workspace_id: String,
    pub project_id: String,
    pub path: String,
    pub kind: FileKind,
    pub size_bytes: u64,
    pub classification: PathClassification,
    pub storage_mode: StorageMode,
    pub hydration_state: HydrationState,
    pub machine_presence: Vec<String>,
    pub policy_version: u64,
    pub snapshot_id: String,
    pub lineage: Vec<String>,
    pub content_id: Option<String>,
    pub access: AccessFlags,
    pub source_watermark: u64,
}

impl NamespaceEntry {
    fn normalized_path(&self) -> String {
        normalize_path(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceTree {
    pub snapshot_id: String,
    pub parent_path: String,
    pub entries: Vec<NamespaceEntry>,
    pub freshness: IndexFreshness,
}

#[derive(Debug, Clone)]
pub struct NamespaceIndex {
    entries: BTreeMap<String, NamespaceEntry>,
    freshness: IndexFreshness,
}

impl NamespaceIndex {
    pub fn new(updated_at: impl Into<String>) -> Self {
        Self {
            entries: BTreeMap::new(),
            freshness: IndexFreshness::from_watermarks(IndexKind::Namespace, 0, 0, updated_at),
        }
    }

    pub fn freshness(&self) -> &IndexFreshness {
        &self.freshness
    }

    pub fn set_freshness(&mut self, freshness: IndexFreshness) {
        self.freshness = freshness;
    }

    pub fn upsert(&mut self, mut entry: NamespaceEntry) {
        entry.path = entry.normalized_path();
        let path = entry.path.clone();
        self.freshness.source_watermark =
            self.freshness.source_watermark.max(entry.source_watermark);
        self.freshness.indexed_watermark =
            self.freshness.indexed_watermark.max(entry.source_watermark);
        self.entries.insert(path, entry);
        if self.freshness.indexed_watermark >= self.freshness.source_watermark {
            self.freshness.readiness = IndexReadiness::Ready;
            self.freshness.reason = None;
        }
    }

    pub fn remove(&mut self, path: &str, source_watermark: u64) -> Option<NamespaceEntry> {
        self.freshness.source_watermark = self.freshness.source_watermark.max(source_watermark);
        self.freshness.indexed_watermark = self.freshness.indexed_watermark.max(source_watermark);
        self.entries.remove(&normalize_path(path))
    }

    pub fn get(&self, path: &str) -> Option<&NamespaceEntry> {
        self.entries.get(&normalize_path(path))
    }

    pub fn list_tree_at_snapshot(&self, snapshot_id: &str, parent_path: &str) -> NamespaceTree {
        let parent = normalize_path(parent_path);
        let entries = self
            .entries
            .values()
            .filter(|entry| entry.snapshot_id == snapshot_id)
            .filter(|entry| entry.access.can_return())
            .filter(|entry| is_direct_child(&entry.path, &parent))
            .cloned()
            .collect();

        NamespaceTree {
            snapshot_id: snapshot_id.to_string(),
            parent_path: parent,
            entries,
            freshness: self.freshness.clone(),
        }
    }

    pub fn path_search(
        &self,
        query: &str,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Vec<NamespaceEntry> {
        let needle = query.to_lowercase();
        let prefix = path_prefix.map(normalize_path);
        self.entries
            .values()
            .filter(|entry| entry.access.can_return())
            .filter(|entry| {
                prefix
                    .as_ref()
                    .is_none_or(|prefix| path_has_prefix(&entry.path, prefix))
            })
            .filter(|entry| entry.path.to_lowercase().contains(&needle))
            .take(limit)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexWriteOutcome {
    Indexed,
    Excluded(DegradedReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDocument {
    pub path: String,
    pub project_id: String,
    pub snapshot_id: String,
    pub content_id: Option<String>,
    pub body: String,
    pub classification: PathClassification,
    pub hydration_state: HydrationState,
    pub policy_summary: String,
    pub access: AccessFlags,
    pub source_watermark: u64,
}

impl TextDocument {
    fn is_indexable(&self) -> Result<(), DegradedReason> {
        if !self.access.policy_readable || !self.access.lease_readable {
            return Err(DegradedReason::PolicyHidden);
        }
        if self.access.generated
            || matches!(
                self.classification,
                PathClassification::Generated | PathClassification::Binary
            )
        {
            return Err(DegradedReason::PolicyHidden);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub path_prefix: Option<String>,
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            path_prefix: None,
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub project_id: String,
    pub snapshot_id: String,
    pub content_id: Option<String>,
    pub snippet: Option<String>,
    pub score: f32,
    pub hydration_state: HydrationState,
    pub policy_summary: String,
    pub freshness: IndexFreshness,
    pub degraded_reason: Option<DegradedReason>,
}

#[derive(Debug, Clone)]
pub struct TextIndex {
    docs: BTreeMap<String, TextDocument>,
    freshness: IndexFreshness,
}

impl TextIndex {
    pub fn new(updated_at: impl Into<String>) -> Self {
        Self {
            docs: BTreeMap::new(),
            freshness: IndexFreshness::from_watermarks(IndexKind::Text, 0, 0, updated_at),
        }
    }

    pub fn freshness(&self) -> &IndexFreshness {
        &self.freshness
    }

    pub fn set_freshness(&mut self, freshness: IndexFreshness) {
        self.freshness = freshness;
    }

    pub fn upsert(&mut self, mut doc: TextDocument) -> IndexWriteOutcome {
        doc.path = normalize_path(&doc.path);
        self.freshness.source_watermark = self.freshness.source_watermark.max(doc.source_watermark);
        self.freshness.indexed_watermark =
            self.freshness.indexed_watermark.max(doc.source_watermark);
        match doc.is_indexable() {
            Ok(()) => {
                self.docs.insert(doc.path.clone(), doc);
                IndexWriteOutcome::Indexed
            }
            Err(reason) => {
                self.docs.remove(&doc.path);
                IndexWriteOutcome::Excluded(reason)
            }
        }
    }

    pub fn remove(&mut self, path: &str, source_watermark: u64) -> Option<TextDocument> {
        self.freshness.source_watermark = self.freshness.source_watermark.max(source_watermark);
        self.freshness.indexed_watermark = self.freshness.indexed_watermark.max(source_watermark);
        self.docs.remove(&normalize_path(path))
    }

    pub fn search(&self, query: &str, options: SearchOptions) -> Vec<SearchHit> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let prefix = options.path_prefix.as_deref().map(normalize_path);
        let candidate_limit = options.offset.saturating_add(options.limit);
        let tantivy_order =
            tantivy_candidate_paths(query, &self.docs, candidate_limit, prefix.as_deref());
        let mut hits = self
            .docs
            .values()
            .filter(|doc| {
                prefix
                    .as_ref()
                    .is_none_or(|prefix| path_has_prefix(&doc.path, prefix))
            })
            .filter_map(|doc| {
                let score = text_score(doc, &terms);
                (score > 0.0).then(|| SearchHit {
                    path: doc.path.clone(),
                    project_id: doc.project_id.clone(),
                    snapshot_id: doc.snapshot_id.clone(),
                    content_id: doc.content_id.clone(),
                    snippet: (doc.hydration_state == HydrationState::Hydrated)
                        .then(|| make_snippet(&doc.body, &terms))
                        .flatten(),
                    score,
                    hydration_state: doc.hydration_state,
                    policy_summary: doc.policy_summary.clone(),
                    freshness: self.freshness.clone(),
                    degraded_reason: (doc.hydration_state != HydrationState::Hydrated)
                        .then_some(DegradedReason::ColdContentNeedsHydration),
                })
            })
            .collect::<Vec<_>>();

        if let Some(order) = tantivy_order {
            hits.sort_by(|left, right| {
                let left_rank = order.get(&left.path).copied().unwrap_or(usize::MAX);
                let right_rank = order.get(&right.path).copied().unwrap_or(usize::MAX);
                left_rank
                    .cmp(&right_rank)
                    .then_with(|| left.path.cmp(&right.path))
            });
        } else {
            hits.sort_by(|left, right| {
                right
                    .score
                    .partial_cmp(&left.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        hits = hits.into_iter().skip(options.offset).collect();
        hits.truncate(options.limit);
        hits
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    TypeScript,
    JavaScript,
    Python,
    Rust,
    Go,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Class,
    Interface,
    Variable,
    Struct,
    Enum,
    Trait,
    Type,
    Import,
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRecord {
    pub name: String,
    pub kind: SymbolKind,
    pub language: Language,
    pub path: String,
    pub project_id: String,
    pub snapshot_id: String,
    pub byte_range: Range<usize>,
    pub line_range: Range<usize>,
    pub parser_status: IndexReadiness,
    pub access: AccessFlags,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDocument {
    pub path: String,
    pub project_id: String,
    pub snapshot_id: String,
    pub language: Language,
    pub source: String,
    pub access: AccessFlags,
    pub source_watermark: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLookupOptions {
    pub path_prefix: Option<String>,
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

impl Default for SymbolLookupOptions {
    fn default() -> Self {
        Self {
            path_prefix: None,
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SymbolIndex {
    records: BTreeMap<String, Vec<SymbolRecord>>,
    freshness: IndexFreshness,
}

impl SymbolIndex {
    pub fn new(updated_at: impl Into<String>) -> Self {
        Self {
            records: BTreeMap::new(),
            freshness: IndexFreshness::from_watermarks(IndexKind::Symbols, 0, 0, updated_at),
        }
    }

    pub fn freshness(&self) -> &IndexFreshness {
        &self.freshness
    }

    pub fn set_freshness(&mut self, freshness: IndexFreshness) {
        self.freshness = freshness;
    }

    pub fn upsert(&mut self, mut doc: SymbolDocument) -> IndexWriteOutcome {
        doc.path = normalize_path(&doc.path);
        self.freshness.source_watermark = self.freshness.source_watermark.max(doc.source_watermark);
        self.freshness.indexed_watermark =
            self.freshness.indexed_watermark.max(doc.source_watermark);
        if !doc.access.can_return() {
            self.records.remove(&doc.path);
            return IndexWriteOutcome::Excluded(DegradedReason::PolicyHidden);
        }
        let records = extract_symbols(&doc);
        self.records.insert(doc.path.clone(), records);
        IndexWriteOutcome::Indexed
    }

    pub fn remove(&mut self, path: &str, source_watermark: u64) -> Option<Vec<SymbolRecord>> {
        self.freshness.source_watermark = self.freshness.source_watermark.max(source_watermark);
        self.freshness.indexed_watermark = self.freshness.indexed_watermark.max(source_watermark);
        self.records.remove(&normalize_path(path))
    }

    pub fn insert_record(&mut self, mut record: SymbolRecord) {
        record.path = normalize_path(&record.path);
        if !record.access.can_return() {
            return;
        }
        self.records
            .entry(record.path.clone())
            .or_default()
            .push(record);
    }

    pub fn records_for_path(&self, path: &str) -> Vec<SymbolRecord> {
        self.records
            .get(&normalize_path(path))
            .cloned()
            .unwrap_or_default()
    }

    pub fn lookup(&self, name: &str, options: SymbolLookupOptions) -> Vec<SymbolRecord> {
        let prefix = options.path_prefix.as_deref().map(normalize_path);
        let mut matches = self
            .records
            .values()
            .flat_map(|records| records.iter())
            .filter(|record| record.name == name)
            .filter(|record| record.access.can_return())
            .filter(|record| {
                prefix
                    .as_ref()
                    .is_none_or(|prefix| path_has_prefix(&record.path, prefix))
            })
            .cloned()
            .collect::<Vec<_>>();

        matches.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.line_range.start.cmp(&right.line_range.start))
                .then_with(|| format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
        });
        matches = matches.into_iter().skip(options.offset).collect();
        matches.truncate(options.limit);
        matches
    }
}

pub fn redact(input: &str) -> String {
    input
        .lines()
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str) -> String {
    if let Some((key, _value)) = secret_assignment(line) {
        return format!("{key}=[REDACTED]");
    }

    let mut redacted = String::new();
    let mut token = String::new();
    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            token.push(ch);
        } else {
            flush_token(&mut redacted, &mut token);
            redacted.push(ch);
        }
    }
    flush_token(&mut redacted, &mut token);
    redacted
}

fn secret_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let (key, value) = trimmed.split_once('=')?;
    let clean_key = key.trim();
    let upper = clean_key.to_ascii_uppercase();
    let sensitive = [
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASS",
        "PRIVATE_KEY",
        "API_KEY",
        "ACCESS_KEY",
        "DATABASE_URL",
    ]
    .iter()
    .any(|needle| upper.contains(needle));
    sensitive.then(|| {
        (
            format!("{}{}", &line[..indent_len], clean_key),
            value.trim().to_string(),
        )
    })
}

fn flush_token(output: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if looks_like_secret_token(token) {
        output.push_str("[REDACTED]");
    } else {
        output.push_str(token);
    }
    token.clear();
}

fn looks_like_secret_token(token: &str) -> bool {
    token.starts_with("sk-")
        || token.starts_with("ghp_")
        || token.starts_with("xoxb-")
        || (token.len() >= 32
            && token
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
                .count()
                >= 28)
}

fn normalize_path(path: &str) -> String {
    path.trim_matches('/').replace('\\', "/")
}

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_direct_child(path: &str, parent: &str) -> bool {
    if parent.is_empty() {
        !path.is_empty() && !path.contains('/')
    } else {
        path.strip_prefix(parent)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    }
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .filter(|term| !term.is_empty())
        .collect()
}

fn text_score(doc: &TextDocument, terms: &[String]) -> f32 {
    let haystack = format!("{}\n{}", doc.path, doc.body).to_lowercase();
    terms
        .iter()
        .map(|term| haystack.match_indices(term).count() as f32)
        .sum()
}

fn make_snippet(body: &str, terms: &[String]) -> Option<String> {
    let lower = body.to_lowercase();
    let start = terms.iter().filter_map(|term| lower.find(term)).min()?;
    let mut from = start.saturating_sub(48);
    let mut to = (start + 96).min(body.len());
    while from > 0 && !body.is_char_boundary(from) {
        from -= 1;
    }
    while to < body.len() && !body.is_char_boundary(to) {
        to += 1;
    }
    Some(redact(body[from..to].trim()))
}

fn extract_symbols(doc: &SymbolDocument) -> Vec<SymbolRecord> {
    let parser_status = parser_status(doc.language, &doc.source);
    let mut records = Vec::new();
    let mut byte_start = 0;
    for (line_index, raw_line) in doc.source.lines().enumerate() {
        let line = raw_line.trim();
        let byte_end = byte_start + raw_line.len();
        let line_range = line_index + 1..line_index + 2;
        let byte_range = byte_start..byte_end;
        match doc.language {
            Language::Rust => extract_rust(line, doc, &byte_range, &line_range, &mut records),
            Language::Python => extract_python(line, doc, &byte_range, &line_range, &mut records),
            Language::TypeScript | Language::JavaScript => {
                extract_js_like(line, doc, &byte_range, &line_range, &mut records)
            }
            Language::Go => extract_go(line, doc, &byte_range, &line_range, &mut records),
        }
        for record in &mut records {
            if record.path == doc.path {
                record.parser_status = parser_status.clone();
            }
        }
        byte_start = byte_end + 1;
    }
    records
}

fn tantivy_candidate_paths(
    query: &str,
    docs: &BTreeMap<String, TextDocument>,
    limit: usize,
    path_prefix: Option<&str>,
) -> Option<BTreeMap<String, usize>> {
    let mut builder = Schema::builder();
    let path_field = builder.add_text_field("path", STRING | STORED);
    let body_field = builder.add_text_field("body", TEXT);
    let schema = builder.build();
    let index = Index::create_in_ram(schema);
    let mut writer = index.writer(16_000_000).ok()?;
    for doc in docs.values().filter(|doc| {
        path_prefix
            .as_ref()
            .is_none_or(|prefix| path_has_prefix(&doc.path, prefix))
    }) {
        writer
            .add_document(doc!(
                path_field => doc.path.clone(),
                body_field => doc.body.clone(),
            ))
            .ok()?;
    }
    writer.commit().ok()?;
    let reader = index.reader().ok()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![path_field, body_field]);
    let parsed = parser.parse_query(query).ok()?;
    let top_docs = searcher
        .search(&parsed, &TopDocs::with_limit(limit.max(1)).order_by_score())
        .ok()?;
    let mut order = BTreeMap::new();
    for (rank, (_score, address)) in top_docs.into_iter().enumerate() {
        let retrieved = searcher.doc::<tantivy::TantivyDocument>(address).ok()?;
        let owned = retrieved
            .get_first(path_field)
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)?;
        order.insert(owned, rank);
    }
    Some(order)
}

fn parser_status(language: Language, source: &str) -> IndexReadiness {
    let mut parser = Parser::new();
    let language = match language {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    };
    if parser.set_language(&language).is_err() {
        return IndexReadiness::Degraded;
    }
    let Some(tree) = parser.parse(source, None) else {
        return IndexReadiness::Degraded;
    };
    if tree.root_node().has_error() {
        IndexReadiness::Degraded
    } else {
        IndexReadiness::Ready
    }
}

fn push_symbol(
    records: &mut Vec<SymbolRecord>,
    doc: &SymbolDocument,
    name: impl Into<String>,
    kind: SymbolKind,
    byte_range: &Range<usize>,
    line_range: &Range<usize>,
) {
    records.push(SymbolRecord {
        name: name.into(),
        kind,
        language: doc.language,
        path: doc.path.clone(),
        project_id: doc.project_id.clone(),
        snapshot_id: doc.snapshot_id.clone(),
        byte_range: byte_range.clone(),
        line_range: line_range.clone(),
        parser_status: IndexReadiness::Ready,
        access: doc.access,
    });
}

fn extract_rust(
    line: &str,
    doc: &SymbolDocument,
    byte_range: &Range<usize>,
    line_range: &Range<usize>,
    records: &mut Vec<SymbolRecord>,
) {
    if let Some((name, kind)) = after_any_kind(
        line,
        &[
            ("pub fn ", SymbolKind::Function),
            ("fn ", SymbolKind::Function),
            ("pub struct ", SymbolKind::Struct),
            ("struct ", SymbolKind::Struct),
            ("pub enum ", SymbolKind::Enum),
            ("enum ", SymbolKind::Enum),
            ("pub trait ", SymbolKind::Trait),
            ("trait ", SymbolKind::Trait),
        ],
    ) {
        push_symbol(records, doc, name, kind, byte_range, line_range);
    }
    if let Some(import) = line
        .strip_prefix("use ")
        .and_then(|rest| rest.trim_end_matches(';').split("::").next())
    {
        push_symbol(
            records,
            doc,
            import,
            SymbolKind::Import,
            byte_range,
            line_range,
        );
    }
}

fn extract_python(
    line: &str,
    doc: &SymbolDocument,
    byte_range: &Range<usize>,
    line_range: &Range<usize>,
    records: &mut Vec<SymbolRecord>,
) {
    if let Some((name, kind)) = after_any_kind(
        line,
        &[
            ("def ", SymbolKind::Function),
            ("async def ", SymbolKind::Function),
            ("class ", SymbolKind::Class),
        ],
    ) {
        push_symbol(records, doc, name, kind, byte_range, line_range);
    }
    if let Some(import) = line
        .strip_prefix("import ")
        .or_else(|| line.strip_prefix("from "))
        && let Some(name) = first_ident(import)
    {
        push_symbol(
            records,
            doc,
            name,
            SymbolKind::Import,
            byte_range,
            line_range,
        );
    }
}

fn extract_js_like(
    line: &str,
    doc: &SymbolDocument,
    byte_range: &Range<usize>,
    line_range: &Range<usize>,
    records: &mut Vec<SymbolRecord>,
) {
    let exported = line.starts_with("export ");
    if let Some((name, kind)) = after_any_kind(
        line,
        &[
            ("export async function ", SymbolKind::Function),
            ("export function ", SymbolKind::Function),
            ("function ", SymbolKind::Function),
            ("export class ", SymbolKind::Class),
            ("class ", SymbolKind::Class),
            ("export interface ", SymbolKind::Interface),
            ("interface ", SymbolKind::Interface),
            ("export const ", SymbolKind::Variable),
            ("const ", SymbolKind::Variable),
            ("export let ", SymbolKind::Variable),
            ("let ", SymbolKind::Variable),
        ],
    ) {
        push_symbol(records, doc, name.clone(), kind, byte_range, line_range);
        if exported {
            push_symbol(
                records,
                doc,
                name,
                SymbolKind::Export,
                byte_range,
                line_range,
            );
        }
    }
    if line.starts_with("import ")
        && let Some(module) = quoted_module(line)
            .or_else(|| first_ident(line.strip_prefix("import ").unwrap_or(line)))
    {
        push_symbol(
            records,
            doc,
            module,
            SymbolKind::Import,
            byte_range,
            line_range,
        );
    }
}

fn extract_go(
    line: &str,
    doc: &SymbolDocument,
    byte_range: &Range<usize>,
    line_range: &Range<usize>,
    records: &mut Vec<SymbolRecord>,
) {
    if let Some((name, kind)) = after_any_kind(
        line,
        &[("func ", SymbolKind::Function), ("type ", SymbolKind::Type)],
    ) {
        push_symbol(records, doc, name, kind, byte_range, line_range);
    }
    if let Some(import) = line.strip_prefix("import ")
        && let Some(module) = quoted_module(import)
    {
        push_symbol(
            records,
            doc,
            module,
            SymbolKind::Import,
            byte_range,
            line_range,
        );
    }
}

fn after_any_kind(line: &str, prefixes: &[(&str, SymbolKind)]) -> Option<(String, SymbolKind)> {
    prefixes.iter().find_map(|(prefix, kind)| {
        line.strip_prefix(prefix)
            .and_then(first_ident)
            .map(|name| (name, *kind))
    })
}

fn first_ident(input: &str) -> Option<String> {
    let ident = input
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '.')
        .collect::<String>();
    (!ident.is_empty()).then_some(ident)
}

fn quoted_module(input: &str) -> Option<String> {
    let start = input.find(['"', '\''])?;
    let quote = input[start..].chars().next()?;
    let rest = &input[start + quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}
