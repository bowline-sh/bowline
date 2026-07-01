use std::ops::Range;

use tree_sitter::Parser;

use super::{IndexReadiness, Language, SymbolDocument, SymbolKind, SymbolRecord};

pub(super) fn extract_symbols(doc: &SymbolDocument) -> Vec<SymbolRecord> {
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
