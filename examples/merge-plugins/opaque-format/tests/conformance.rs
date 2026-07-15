use bowline_merge_plugin_testkit::{ConformanceCase, built_wasm_path, round_trip};

#[test]
fn wasm_round_trips_through_host_abi() {
    let wasm = built_wasm_path(
        env!("CARGO_MANIFEST_DIR"),
        "bowline_opaque_format_merge_plugin",
    );

    round_trip(
        &wasm,
        &ConformanceCase {
            path: "asset.opaque",
            base: include_bytes!("../fixtures/base.opaque"),
            local: include_bytes!("../fixtures/local.opaque"),
            remote: include_bytes!("../fixtures/remote.opaque"),
            expected_merge: Some(include_bytes!("../fixtures/expected.opaque")),
        },
    )
    .unwrap_or_else(|error| panic!("single changed side conforms to the host ABI: {error}"));
    round_trip(
        &wasm,
        &ConformanceCase {
            path: "asset.opaque",
            base: include_bytes!("../fixtures/base.opaque"),
            local: include_bytes!("../fixtures/both-local.opaque"),
            remote: include_bytes!("../fixtures/both-remote.opaque"),
            expected_merge: None,
        },
    )
    .unwrap_or_else(|error| panic!("two changed sides decline through the host ABI: {error}"));
}
