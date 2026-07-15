use bowline_testkit::{CostBudget, CostReport, SyncScenario};

#[test]
fn small_sync_stays_within_deterministic_budget() {
    let scenario = SyncScenario::new("cost-budget-pass").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");

    let _outcome = scenario.tick().expect("sync tick");
    let report = scenario.cost_report();

    CostBudget {
        max_put_count: Some(8),
        max_files_hashed: Some(4),
        max_control_plane_upload_intents: Some(8),
        ..Default::default()
    }
    .assert_report(&report)
    .expect("budget");
}

#[test]
fn budget_failure_names_metric_observed_and_cap() {
    let report = CostReport {
        byte_store: Default::default(),
        scan: Default::default(),
        control_plane_upload_intents: 7,
        peak_memory_bytes: Some(1_000_000),
    };

    let error = CostBudget {
        max_control_plane_upload_intents: Some(3),
        ..Default::default()
    }
    .assert_report(&report)
    .expect_err("budget fails");

    assert_eq!(error.metric, "control_plane_upload_intents");
    assert_eq!(error.observed, 7);
    assert_eq!(error.cap, 3);
}

#[test]
fn peak_memory_budget_uses_the_observed_high_water_mark() {
    let report = CostReport {
        byte_store: Default::default(),
        scan: Default::default(),
        control_plane_upload_intents: 0,
        peak_memory_bytes: Some(8 * 1024 * 1024),
    };

    let error = CostBudget {
        max_peak_memory_bytes: Some(4 * 1024 * 1024),
        ..Default::default()
    }
    .assert_report(&report)
    .expect_err("memory budget fails");

    assert_eq!(error.metric, "peak_memory_bytes");
    assert_eq!(error.observed, 8 * 1024 * 1024);
    assert_eq!(error.cap, 4 * 1024 * 1024);
}
