# Merge plugin examples

These are starter templates for Bowline's current Wasm merge-plugin ABI. They
are static examples, not a registry, marketplace, SDK, or scaffold generator.

Each example is a standalone Rust project that opts out of the root Cargo
workspace. Install the Wasm target once, then build one with:

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/merge-plugins/<name>/Cargo.toml \
  --target wasm32-unknown-unknown --release
```

Each directory includes:

- `src/lib.rs`: minimal `bowline_alloc`, `bowline_merge`, and `bowline_validate`
  exports.
- `.bowlinemerge.toml`: a sample project policy with a placeholder digest.
- `fixtures/`: tiny base/local/remote examples for the behavior.
- `README.md`: build, digest, configure, approve, and test steps.

The current ABI is intentionally small:

```text
export memory
export bowline_alloc(len) -> ptr
export bowline_merge(base_ptr, base_len, local_ptr, local_len,
                     remote_ptr, remote_len, path_ptr, path_len) -> i64
export bowline_validate(candidate_ptr, candidate_len, path_ptr, path_len) -> i32
```

Bowline calls `bowline_alloc` before writing base/local/remote/path input bytes
into guest memory. Successful merge output is packed as
`(candidate_ptr << 32) | candidate_len`. Negative return values mean `no-merge`,
which Bowline treats as a normal sync conflict.

## Runtime budgets

The host applies three hard ceilings to every Wasm merge invocation: 5_000_000
fuel units, 64 MiB maximum linear memory, and 32 MiB maximum output bytes. These
budgets are not configurable in this release. Exceeding fuel creates a distinct
merge-plugin conflict: the plugin exceeded its compute budget.

## Conformance tests

Each example has a `tests/conformance.rs` test that drives the built Wasm module
through the same ABI shape as the host: `bowline_alloc`, packed merge output,
and `bowline_validate`. Build the Wasm module first, then run the test:

```sh
cargo build --manifest-path examples/merge-plugins/<name>/Cargo.toml \
  --target wasm32-unknown-unknown --release
cargo test --manifest-path examples/merge-plugins/<name>/Cargo.toml \
  --test conformance -- --nocapture
```
