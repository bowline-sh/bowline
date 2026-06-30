# Contract Fixtures

Phase 0B uses hand-written JSON fixtures as the cross-language schema source of
truth. TypeScript owns the exported vocabulary in `packages/contracts`, and Rust
serde tests in `crates/bowline-core` must round-trip these fixtures without changing
their JSON shape.

When a shared contract changes, update the fixture first, then update the
TypeScript and Rust domain types until both contract test suites pass.
