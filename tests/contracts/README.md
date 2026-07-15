# Contract Fixtures

Phase 0B uses hand-written JSON fixtures as the cross-language schema source of
truth. TypeScript owns the exported vocabulary in `packages/contracts`, and Rust
serde tests in `crates/bowline-core` must round-trip these fixtures without
changing their JSON shape.

`manifest.json` is the fixture coverage index. It lists every JSON and NDJSON
fixture under this directory, the fixture family, the relative path, the file
format, the expected contract kind, and the language decoders that must consume
the fixture. The manifest is not a schema source of truth; it keeps Rust,
TypeScript, and Swift tests from drifting away from the hand-written fixtures.

When a shared contract changes, update the fixture first, then update the
TypeScript and Rust domain types until both contract test suites pass.

The `proofs/` fixtures pin device-authorization proof subjects for the hosted
Rust control-plane client and Convex proof builders.
