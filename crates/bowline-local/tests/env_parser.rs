use bowline_local::env::{EnvLineKind, QuoteStyle, parse_env_text};

#[test]
fn parser_preserves_env_layout_and_occurrences_without_debug_leaks() {
    let parsed = parse_env_text(
        "app/.env.local",
        "local",
        b"# hello\n\nexport API_KEY=\"super-secret\" # keep\nAPI_KEY=second\nEMPTY=\nnot a valid line\n",
    );

    assert_eq!(parsed.source_path, "app/.env.local");
    assert_eq!(parsed.profile, "local");
    assert!(matches!(parsed.lines[0].kind, EnvLineKind::Comment));
    assert!(matches!(parsed.lines[1].kind, EnvLineKind::Blank));

    let first = match &parsed.lines[2].kind {
        EnvLineKind::KeyValue(value) => value,
        other => panic!("expected key value, got {other:?}"),
    };
    assert_eq!(parsed.lines[2].line_number, 3);
    assert_eq!(first.key, "API_KEY");
    assert_eq!(first.occurrence_index, 0);
    assert!(first.export_prefix);
    assert_eq!(first.quote_style, QuoteStyle::Double);
    assert_eq!(first.value.as_bytes(), b"super-secret");

    let second = match &parsed.lines[3].kind {
        EnvLineKind::KeyValue(value) => value,
        other => panic!("expected key value, got {other:?}"),
    };
    assert_eq!(second.key, "API_KEY");
    assert_eq!(second.occurrence_index, 1);

    let empty = match &parsed.lines[4].kind {
        EnvLineKind::KeyValue(value) => value,
        other => panic!("expected key value, got {other:?}"),
    };
    assert_eq!(empty.key, "EMPTY");
    assert_eq!(empty.value.as_bytes(), b"");

    let opaque = match &parsed.lines[5].kind {
        EnvLineKind::Opaque(line) => line,
        other => panic!("expected opaque line, got {other:?}"),
    };
    assert_eq!(opaque.bytes.as_bytes(), b"not a valid line");

    let debug = format!("{parsed:?}");
    assert!(!debug.contains("super-secret"));
    assert!(!debug.contains("not a valid line"));
    assert!(debug.contains("[redacted]"));
}

#[test]
fn parser_preserves_escaped_double_quotes_in_values() {
    let parsed = parse_env_text(
        "app/.env.local",
        "local",
        br#"QUOTED="old \"secret\"" # keep
"#,
    );

    let value = match &parsed.lines[0].kind {
        EnvLineKind::KeyValue(value) => value,
        other => panic!("expected key value, got {other:?}"),
    };
    assert_eq!(value.key, "QUOTED");
    assert_eq!(value.quote_style, QuoteStyle::Double);
    assert_eq!(value.value.as_bytes(), br#"old \"secret\""#);
}
