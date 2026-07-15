use bowline_core::ids::{DeviceId, LeaseId, ProjectId};
use bowline_local::env::{
    EnvLineKind, EnvProviderRecord, EnvProviderRequest, EnvReadScope, EnvRecordFreshness,
    EnvRecordRestriction, QuoteStyle, SecretBytes, parse_env_text, resolve_env_provider_request,
};

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

#[test]
fn provider_returns_last_effective_values_and_redacted_denials() {
    let project_id = ProjectId::new("project-ok");
    let caller = DeviceId::new("device-a");
    let records = vec![
        record(
            &project_id,
            "app/.env",
            "API_KEY",
            0,
            "old",
            EnvRecordRestriction::Inherited,
        ),
        record(
            &project_id,
            "app/.env",
            "API_KEY",
            1,
            "last-secret",
            EnvRecordRestriction::Inherited,
        ),
        record(
            &project_id,
            "app/.env",
            "ADMIN_TOKEN",
            0,
            "admin-secret",
            EnvRecordRestriction::Restricted {
                allowed_device_ids: vec![DeviceId::new("device-b")],
                lease_only: false,
            },
        ),
        record(
            &project_id,
            "app/.env",
            "LOCAL_SOCKET",
            0,
            "/tmp/app.sock",
            EnvRecordRestriction::MachineLocal,
        ),
        record(
            &project_id,
            "app/.env",
            "OVERRIDDEN_TOKEN",
            0,
            "old-visible",
            EnvRecordRestriction::Inherited,
        ),
        record(
            &project_id,
            "app/.env",
            "OVERRIDDEN_TOKEN",
            1,
            "new-restricted",
            EnvRecordRestriction::Restricted {
                allowed_device_ids: vec![DeviceId::new("device-b")],
                lease_only: false,
            },
        ),
        EnvProviderRecord {
            freshness: EnvRecordFreshness::Stale,
            ..record(
                &project_id,
                "app/.env",
                "STALE_TOKEN",
                0,
                "stale-secret",
                EnvRecordRestriction::Inherited,
            )
        },
        record(
            &ProjectId::new("other-project"),
            "other/.env",
            "OTHER_TOKEN",
            0,
            "other-secret",
            EnvRecordRestriction::Inherited,
        ),
    ];
    let request = EnvProviderRequest {
        caller_device_id: Some(caller),
        lease_id: Some(LeaseId::new("lease-a")),
        project_id,
        read_scope: EnvReadScope::Lease,
        profile: "local".to_string(),
    };

    let response = resolve_env_provider_request(&request, &records);

    assert_eq!(response.values["API_KEY"].as_bytes(), b"last-secret");
    assert!(!response.values.contains_key("ADMIN_TOKEN"));
    assert!(!response.values.contains_key("OVERRIDDEN_TOKEN"));
    assert!(!response.values.contains_key("STALE_TOKEN"));
    assert!(!response.values.contains_key("LOCAL_SOCKET"));
    assert_eq!(response.denials.len(), 5);
    assert!(response.denials.iter().any(|denial| {
        denial.key == "LOCAL_SOCKET"
            && denial.reason == bowline_local::env::EnvProviderDenialReason::MachineLocal
    }));

    let debug = format!("{response:?}");
    assert!(!debug.contains("last-secret"));
    assert!(!debug.contains("admin-secret"));
    assert!(!debug.contains("old-visible"));
    assert!(!debug.contains("new-restricted"));
    assert!(!debug.contains("stale-secret"));
    assert!(debug.contains("ADMIN_TOKEN"));
}

#[test]
fn provider_denies_missing_caller_without_value_leak() {
    let project_id = ProjectId::new("project-ok");
    let request = EnvProviderRequest {
        caller_device_id: None,
        lease_id: None,
        project_id: project_id.clone(),
        read_scope: EnvReadScope::Project,
        profile: "local".to_string(),
    };

    let response = resolve_env_provider_request(
        &request,
        &[record(
            &project_id,
            "app/.env",
            "API_KEY",
            0,
            "caller-secret",
            EnvRecordRestriction::Inherited,
        )],
    );

    assert!(response.values.is_empty());
    assert_eq!(
        response.denials[0].reason,
        bowline_local::env::EnvProviderDenialReason::MissingCaller
    );
    assert!(!format!("{response:?}").contains("caller-secret"));
}

fn record(
    project_id: &ProjectId,
    source_path: &str,
    key: &str,
    occurrence_index: usize,
    value: &str,
    restriction: EnvRecordRestriction,
) -> EnvProviderRecord {
    EnvProviderRecord {
        project_id: project_id.clone(),
        source_path: source_path.to_string(),
        profile: "local".to_string(),
        key: key.to_string(),
        occurrence_index,
        value: SecretBytes::from(value),
        restriction,
        freshness: EnvRecordFreshness::Fresh,
    }
}
