# Opaque format merge plugin

This is a skeleton for binary, opaque, or project-specific formats. It is
conservative by design: it returns a candidate only when exactly one side
changed from base and the changed bytes still have the expected file magic.

Use this when a format needs custom validation but has no safe merge story yet.
Add real parsing and semantic checks before accepting simultaneous edits.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/merge-plugins/opaque-format/Cargo.toml \
  --target wasm32-unknown-unknown --release
```

The Wasm module will be:

```text
examples/merge-plugins/opaque-format/target/wasm32-unknown-unknown/release/bowline_opaque_format_merge_plugin.wasm
```

## Digest

```sh
b3sum examples/merge-plugins/opaque-format/target/wasm32-unknown-unknown/release/bowline_opaque_format_merge_plugin.wasm
```

Use the first field as `blake3:<digest>`.

## Configure

```sh
mkdir -p ~/Code/.bowline/plugins
cp examples/merge-plugins/opaque-format/target/wasm32-unknown-unknown/release/bowline_opaque_format_merge_plugin.wasm \
  ~/Code/.bowline/plugins/opaque-format.wasm
cp examples/merge-plugins/opaque-format/.bowlinemerge.toml ~/Code/.bowlinemerge.toml
```

Replace the placeholder digest in `~/Code/.bowlinemerge.toml`.

## Approve

Run `bowline status` after syncing the policy to see the `policy.needs_approval`
event. Then approve the declared tuple:

```sh
bowline device approve --merge-plugin --root ~/Code \
  --id opaque-format \
  --plugin-version 0.1.0 \
  --digest blake3:<module-digest> \
  --yes
```

Bowline derives the matcher and validator versions from `.bowlinemerge.toml`.
Passing `--matcher-version` or `--validator-version` is optional; if supplied,
the value must match the declaration. Approval refuses a tuple matching no
declaration and lists the approvable tuples.

## Test

```sh
cargo test --manifest-path examples/merge-plugins/opaque-format/Cargo.toml
```

The conformance test drives the built Wasm module through the host ABI:

```sh
cargo build --manifest-path examples/merge-plugins/opaque-format/Cargo.toml \
  --target wasm32-unknown-unknown --release
cargo test --manifest-path examples/merge-plugins/opaque-format/Cargo.toml \
  --test conformance -- --nocapture
```

`*.opaque` is a plugin-owned format, so the host does not independently re-parse
the file. Validity rests on this plugin's `bowline_validate`; Bowline's
host-side mitigations are double-run determinism and digest pinning in the
approval identity.

Fixtures:

- `fixtures/base.opaque`: common base with `OPAQ` magic.
- `fixtures/local.opaque`: one safe local edit.
- `fixtures/remote.opaque`: unchanged remote.
- `fixtures/expected.opaque`: accepted output.
- `fixtures/both-local.opaque` and `fixtures/both-remote.opaque`: both changed,
  so the plugin refuses.
