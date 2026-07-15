# Jupyter notebook merge plugin

This template demonstrates the reason merge plugins exist: notebooks can merge
when independent cells changed, but should refuse same-cell edits.

The implementation is deliberately small and aligned to the current ABI. It
expects pretty-printed notebook JSON like the fixtures, identifies cell blocks
by their `"id"` fields, and splices local cell blocks over the remote notebook
only when local and remote changed disjoint cell IDs.

It is not a complete notebook merger. Treat it as a useful starting point for a
real parser-backed plugin.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/merge-plugins/jupyter-notebook/Cargo.toml \
  --target wasm32-unknown-unknown --release
```

The Wasm module will be:

```text
examples/merge-plugins/jupyter-notebook/target/wasm32-unknown-unknown/release/bowline_jupyter_notebook_merge_plugin.wasm
```

## Digest

```sh
b3sum examples/merge-plugins/jupyter-notebook/target/wasm32-unknown-unknown/release/bowline_jupyter_notebook_merge_plugin.wasm
```

Use the first field as `blake3:<digest>`.

## Configure

```sh
mkdir -p ~/Code/.bowline/plugins
cp examples/merge-plugins/jupyter-notebook/target/wasm32-unknown-unknown/release/bowline_jupyter_notebook_merge_plugin.wasm \
  ~/Code/.bowline/plugins/jupyter-notebook.wasm
cp examples/merge-plugins/jupyter-notebook/.bowlinemerge.toml ~/Code/.bowlinemerge.toml
```

Replace the placeholder digest in `~/Code/.bowlinemerge.toml`.

## Approve

Run `bowline status` after syncing the policy to see the `policy.needs_approval`
event. Then approve the declared tuple:

```sh
bowline device approve --merge-plugin --root ~/Code \
  --id jupyter-notebook \
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
cargo test --manifest-path examples/merge-plugins/jupyter-notebook/Cargo.toml
```

The conformance test drives the built Wasm module through the host ABI:

```sh
cargo build --manifest-path examples/merge-plugins/jupyter-notebook/Cargo.toml \
  --target wasm32-unknown-unknown --release
cargo test --manifest-path examples/merge-plugins/jupyter-notebook/Cargo.toml \
  --test conformance -- --nocapture
```

`*.ipynb` is a plugin-owned format, so the host does not independently re-parse
the notebook. Validity rests on this plugin's `bowline_validate`; Bowline's
host-side mitigations are double-run determinism and digest pinning in the
approval identity.

Fixtures:

- `fixtures/base.ipynb`: common base with two cells.
- `fixtures/local.ipynb`: local edits the markdown cell.
- `fixtures/remote.ipynb`: remote edits the code cell.
- `fixtures/expected.ipynb`: accepted disjoint-cell merge.
- `fixtures/conflict-local.ipynb` and `fixtures/conflict-remote.ipynb`: both
  edit the same cell and should be refused.
