# JSON validator merge plugin

This is the smallest useful merge-plugin template. It accepts a candidate only
when exactly one side changed, or when both sides produced identical bytes, and
the candidate still looks like JSON.

It is intentionally dependency-free. The validator is a compact structural check
for starter use; replace it with a real parser before trusting a richer format.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/merge-plugins/json-validator/Cargo.toml \
  --target wasm32-unknown-unknown --release
```

The Wasm module will be:

```text
examples/merge-plugins/json-validator/target/wasm32-unknown-unknown/release/bowline_json_validator_merge_plugin.wasm
```

## Digest

Bowline expects a `blake3:` digest for the exact Wasm bytes. If `b3sum` is
installed:

```sh
b3sum examples/merge-plugins/json-validator/target/wasm32-unknown-unknown/release/bowline_json_validator_merge_plugin.wasm
```

Use the first field as `blake3:<digest>` in `.bowlinemerge.toml`.

## Configure

Copy the module into the project that owns the merge policy:

```sh
mkdir -p ~/Code/.bowline/plugins
cp examples/merge-plugins/json-validator/target/wasm32-unknown-unknown/release/bowline_json_validator_merge_plugin.wasm \
  ~/Code/.bowline/plugins/json-validator.wasm
cp examples/merge-plugins/json-validator/.bowlinemerge.toml ~/Code/.bowlinemerge.toml
```

Replace the placeholder digest in `~/Code/.bowlinemerge.toml`.

## Approve

Run `bowline status` after syncing the policy to see the `policy.needs_approval`
event. Then approve the declared tuple:

```sh
bowline device approve --merge-plugin --root ~/Code \
  --id json-validator \
  --plugin-version 0.1.0 \
  --digest blake3:<module-digest> \
  --yes
```

Bowline derives the matcher and validator versions from `.bowlinemerge.toml`.
Passing `--matcher-version` or `--validator-version` is optional; if supplied,
the value must match the declaration. Approval refuses a tuple matching no
declaration and lists the approvable tuples.

## Test

Native unit tests exercise the starter merge rule:

```sh
cargo test --manifest-path examples/merge-plugins/json-validator/Cargo.toml
```

The conformance test drives the built Wasm module through the host ABI:

```sh
cargo build --manifest-path examples/merge-plugins/json-validator/Cargo.toml \
  --target wasm32-unknown-unknown --release
cargo test --manifest-path examples/merge-plugins/json-validator/Cargo.toml \
  --test conformance -- --nocapture
```

For `*.json`, the host independently re-parses merged output after the plugin
validates it. For plugin-owned formats, validity rests on the plugin's own
`bowline_validate`; Bowline's host-side mitigations there are double-run
determinism and digest pinning in the approval identity.

Fixtures:

- `fixtures/base.json`: common base.
- `fixtures/local.json`: one safe local edit.
- `fixtures/remote.json`: unchanged remote.
- `fixtures/expected.json`: accepted output.
- `fixtures/invalid-local.json`: refused because the changed side is invalid.
