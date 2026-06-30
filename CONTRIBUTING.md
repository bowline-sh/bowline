# Contributing

Bowline is public-readable first. The public source repo exists so developers
can inspect the local client, daemon, sync engine, storage substrate, contracts,
tests, and trust-boundary docs.

The private repo remains canonical. Public source changes should start in the
private repo and flow through the generated public export; do not edit generated
public source as a second source of truth.

## Pull Requests

We are not accepting unsolicited feature PRs yet. Small correctness fixes,
documentation fixes, and security-adjacent clarifications may be considered, but
maintainers may close broader feature work until the external contribution flow
is intentionally opened.

Forked PR CI is maintainer-approved. Do not expect untrusted CI to run
automatically.

## Issues

Useful bug reports include:

- exact Bowline version or commit
- platform and architecture
- command or workflow that failed
- sanitized diagnostic output
- expected behavior and observed behavior
- whether the issue touches device trust, env sync, Git state, hydration, or
  agent leases

Do not post secrets, raw `.env` values, Recovery Key words, device private keys,
workspace keys, or private repository contents in public issues.
