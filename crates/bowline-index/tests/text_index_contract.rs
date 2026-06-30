use bowline_index::{
    AccessFlags, HydrationState, IndexWriteOutcome, PathClassification, SearchOptions,
    TextDocument, TextIndex, redact,
};

fn doc(path: &str, body: &str) -> TextDocument {
    TextDocument {
        path: path.to_string(),
        project_id: "proj_acme_web".to_string(),
        snapshot_id: "snap_1".to_string(),
        content_id: Some(format!("cid_{path}")),
        body: body.to_string(),
        classification: PathClassification::Source,
        hydration_state: HydrationState::Hydrated,
        policy_summary: "policy:readable lease:readable".to_string(),
        access: AccessFlags::readable(),
        source_watermark: 1,
    }
}

#[test]
fn text_search_returns_stable_redacted_hits() {
    let mut index = TextIndex::new("2026-06-25T10:00:00Z");
    assert_eq!(
        index.upsert(doc(
            "src/auth/callback.ts",
            "export function authCallback() {\n  const token = \"sk-live-secret-secret-secret-secret\";\n  return token;\n}"
        )),
        IndexWriteOutcome::Indexed
    );

    let hits = index.search(
        "authCallback",
        SearchOptions {
            path_prefix: Some("src".to_string()),
            limit: 5,
            offset: 0,
        },
    );

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/auth/callback.ts");
    assert!(hits[0].snippet.as_ref().unwrap().contains("[REDACTED]"));
    assert!(!hits[0].snippet.as_ref().unwrap().contains("sk-live"));
}

#[test]
fn text_search_excludes_generated_binary_and_hidden_documents() {
    let mut index = TextIndex::new("2026-06-25T10:00:00Z");
    let mut generated = doc("dist/auth.js", "authCallback");
    generated.classification = PathClassification::Generated;
    generated.access.generated = true;
    assert!(matches!(
        index.upsert(generated),
        IndexWriteOutcome::Excluded(_)
    ));

    let mut hidden = doc(".env.local", "AUTH_TOKEN=secret");
    hidden.access = AccessFlags::hidden();
    assert!(matches!(
        index.upsert(hidden),
        IndexWriteOutcome::Excluded(_)
    ));

    assert!(
        index
            .search("authCallback", SearchOptions::default())
            .is_empty()
    );
}

#[test]
fn cold_text_hit_is_truthful_about_hydration() {
    let mut index = TextIndex::new("2026-06-25T10:00:00Z");
    let mut cold = doc("src/auth/callback.ts", "auth callback");
    cold.hydration_state = HydrationState::Cold;
    index.upsert(cold);

    let hits = index.search("callback", SearchOptions::default());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].snippet, None);
    assert!(hits[0].degraded_reason.is_some());
}

#[test]
fn text_search_offset_pages_do_not_skip_twice() {
    let mut index = TextIndex::new("2026-06-25T10:00:00Z");
    index.upsert(doc("src/one.ts", "needle alpha alpha alpha"));
    index.upsert(doc("src/two.ts", "needle beta beta"));
    index.upsert(doc("src/three.ts", "needle gamma"));

    let first_page = index.search(
        "needle",
        SearchOptions {
            path_prefix: Some("src".to_string()),
            limit: 2,
            offset: 0,
        },
    );
    let second_page = index.search(
        "needle",
        SearchOptions {
            path_prefix: Some("src".to_string()),
            limit: 2,
            offset: 2,
        },
    );

    assert_eq!(first_page.len(), 2);
    assert_eq!(second_page.len(), 1);
    assert!(!first_page.iter().any(|hit| hit.path == second_page[0].path));
}

#[test]
fn text_search_keeps_path_only_matches_outside_tantivy_candidates() {
    let mut index = TextIndex::new("2026-06-25T10:00:00Z");
    index.upsert(doc("src/auth/callback.ts", "exports callback handler"));

    let hits = index.search(
        "auth",
        SearchOptions {
            path_prefix: Some("src".to_string()),
            limit: 5,
            offset: 0,
        },
    );

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/auth/callback.ts");
}

#[test]
fn redaction_masks_env_assignments_and_secret_tokens() {
    let redacted = redact("AUTH_TOKEN=abc123\nplain sk-test-abcdefghijklmnopqrstuvwxyz123456");
    assert!(redacted.contains("AUTH_TOKEN=[REDACTED]"));
    assert!(!redacted.contains("abc123"));
    assert!(!redacted.contains("sk-test"));
}
