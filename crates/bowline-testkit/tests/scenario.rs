use bowline_testkit::{SyncScenario, TwoDeviceSyncScenario};

#[test]
fn seeded_scenario_runs_one_real_sync_tick() {
    let scenario = SyncScenario::new("seeded-scenario").expect("scenario");
    scenario
        .workspace()
        .create_git_repo("app")
        .expect("git fixture");
    scenario
        .workspace()
        .write_project_file("app", "src/main.rs", b"fn main() {}\n")
        .expect("fixture file");

    let _outcome = scenario.tick().expect("sync tick");

    scenario.assert_invariants().expect("invariants");
    assert!(scenario.cost_report().byte_store.put_count > 0);
}

#[test]
fn two_device_extension_hook_constructs_shared_wiring() {
    let scenario = TwoDeviceSyncScenario::new("two-device-extension").expect("scenario");

    scenario
        .first()
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let _outcome = scenario.first().tick().expect("first sync tick");
    scenario
        .first()
        .assert_invariants()
        .expect("first invariants");

    let _outcome = scenario.second().tick().expect("second sync tick");
    scenario
        .second()
        .assert_invariants()
        .expect("second invariants");
}
