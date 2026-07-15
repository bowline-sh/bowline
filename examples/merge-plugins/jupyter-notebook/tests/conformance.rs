use bowline_merge_plugin_testkit::{ConformanceCase, built_wasm_path, round_trip};

#[test]
fn wasm_round_trips_through_host_abi() {
    let wasm = built_wasm_path(
        env!("CARGO_MANIFEST_DIR"),
        "bowline_jupyter_notebook_merge_plugin",
    );

    round_trip(
        &wasm,
        &ConformanceCase {
            path: "analysis.ipynb",
            base: include_bytes!("../fixtures/base.ipynb"),
            local: include_bytes!("../fixtures/local.ipynb"),
            remote: include_bytes!("../fixtures/remote.ipynb"),
            expected_merge: Some(include_bytes!("../fixtures/expected.ipynb")),
        },
    )
    .unwrap_or_else(|error| panic!("disjoint cell edits conform to the host ABI: {error}"));
    round_trip(
        &wasm,
        &ConformanceCase {
            path: "analysis.ipynb",
            base: include_bytes!("../fixtures/base.ipynb"),
            local: include_bytes!("../fixtures/conflict-local.ipynb"),
            remote: include_bytes!("../fixtures/conflict-remote.ipynb"),
            expected_merge: None,
        },
    )
    .unwrap_or_else(|error| panic!("same-cell edits decline through the host ABI: {error}"));
}
