use bowline_merge_plugin_testkit::{ConformanceCase, built_wasm_path, round_trip};

#[test]
fn wasm_round_trips_through_host_abi() {
    let wasm = built_wasm_path(
        env!("CARGO_MANIFEST_DIR"),
        "bowline_json_validator_merge_plugin",
    );

    round_trip(
        &wasm,
        &ConformanceCase {
            path: "example.json",
            base: include_bytes!("../fixtures/base.json"),
            local: include_bytes!("../fixtures/local.json"),
            remote: include_bytes!("../fixtures/remote.json"),
            expected_merge: Some(include_bytes!("../fixtures/expected.json")),
        },
    )
    .unwrap_or_else(|error| panic!("happy-path merge conforms to the host ABI: {error}"));
    round_trip(
        &wasm,
        &ConformanceCase {
            path: "example.json",
            base: include_bytes!("../fixtures/base.json"),
            local: include_bytes!("../fixtures/invalid-local.json"),
            remote: include_bytes!("../fixtures/remote.json"),
            expected_merge: None,
        },
    )
    .unwrap_or_else(|error| panic!("invalid JSON side declines through the host ABI: {error}"));
}
