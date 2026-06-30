use bowline_index::{
    AccessFlags, IndexWriteOutcome, Language, SymbolDocument, SymbolIndex, SymbolKind,
    SymbolLookupOptions,
};

fn source(path: &str, language: Language, body: &str) -> SymbolDocument {
    SymbolDocument {
        path: path.to_string(),
        project_id: "proj_acme_web".to_string(),
        snapshot_id: "snap_1".to_string(),
        language,
        source: body.to_string(),
        access: AccessFlags::readable(),
        source_watermark: 1,
    }
}

#[test]
fn symbol_lookup_finds_typescript_definitions_imports_and_exports() {
    let mut index = SymbolIndex::new("2026-06-25T10:00:00Z");
    assert_eq!(
        index.upsert(source(
            "src/session.ts",
            Language::TypeScript,
            "import { db } from \"./db\";\nexport function createSession() {\n  return db;\n}"
        )),
        IndexWriteOutcome::Indexed
    );

    let defs = index.lookup("createSession", SymbolLookupOptions::default());
    assert_eq!(defs.len(), 2);
    assert!(
        defs.iter()
            .any(|record| record.kind == SymbolKind::Function)
    );
    assert!(defs.iter().any(|record| record.kind == SymbolKind::Export));

    let imports = index.lookup("./db", SymbolLookupOptions::default());
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].kind, SymbolKind::Import);
}

#[test]
fn symbol_lookup_supports_rust_python_and_go_shapes() {
    let mut index = SymbolIndex::new("2026-06-25T10:00:00Z");
    index.upsert(source(
        "src/lib.rs",
        Language::Rust,
        "pub fn create_session() {}",
    ));
    index.upsert(source(
        "app.py",
        Language::Python,
        "def create_session():\n    pass",
    ));
    index.upsert(source("main.go", Language::Go, "func CreateSession() {}"));

    assert_eq!(
        index
            .lookup("create_session", SymbolLookupOptions::default())
            .len(),
        2
    );
    assert_eq!(
        index
            .lookup("CreateSession", SymbolLookupOptions::default())
            .len(),
        1
    );
}

#[test]
fn hidden_symbol_documents_purge_existing_records() {
    let mut index = SymbolIndex::new("2026-06-25T10:00:00Z");
    index.upsert(source(
        "src/session.ts",
        Language::TypeScript,
        "function createSession() {}",
    ));

    let mut hidden = source(
        "src/session.ts",
        Language::TypeScript,
        "function createSession() {}",
    );
    hidden.access = AccessFlags::hidden();
    assert!(matches!(
        index.upsert(hidden),
        IndexWriteOutcome::Excluded(_)
    ));

    assert!(
        index
            .lookup("createSession", SymbolLookupOptions::default())
            .is_empty()
    );
}

#[test]
fn malformed_symbol_documents_keep_file_scoped_parser_degradation() {
    let mut index = SymbolIndex::new("2026-06-25T10:00:00Z");
    index.upsert(source(
        "src/broken.ts",
        Language::TypeScript,
        "export function createSession( {",
    ));

    let records = index.lookup("createSession", SymbolLookupOptions::default());
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.parser_status == bowline_index::IndexReadiness::Degraded)
    );
}
