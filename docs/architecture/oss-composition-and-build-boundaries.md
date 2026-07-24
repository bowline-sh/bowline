# OSS composition and build boundaries

This document records what `bowline` uses from open source, what it only
studies, and what it must build itself.

## Decision: Do not adopt an existing OSS project wholesale

There is no open-source project that can become `bowline` without bending the
product thesis. The closest projects solve important slices, but not the whole
workspace promise.

The product promise is:

```text
Your ~/Code tree exists everywhere as a real directory, syncs source/config/env,
understands dev junk, carries project env, and gives every agent a fresh
workspace.
```

That requires a custom workspace graph, policy model, trust/status surface,
project env sync model, hydration semantics, and agent lease runtime.

Use open source aggressively for plumbing. Do not let a plumbing project become
the product model.

## Public source boundary

The public repository is an allowlisted client/core source export from the
private canonical repo. It exists so developers can inspect the CLI, daemon,
local sync engine, storage substrate, contracts, safe tests, and trust-boundary
documentation.

The export does not make the public repo canonical, does not publish prerelease
agent instructions, and does not imply self-hosting or open-ended contribution
support. Runtime and package deployment channels such as Convex, Cloudflare,
crates.io, npm, release assets, and app distribution remain separate release
work.

## Use for plumbing

| Area                          | Use first                                                                                                                                                  | Why                                                                                                              |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Local daemon                  | Rust, Tokio                                                                                                                                                | Reliable async client runtime.                                                                                   |
| Local state                   | SQLite WAL via `rusqlite` or SQLx                                                                                                                          | Inspectable, repairable, and enough for first local metadata.                                                    |
| Web and dashboard             | TanStack Start on Cloudflare Workers                                                                                                                       | Account, billing, status, and support surfaces without making the dashboard the product center.                  |
| Account identity              | WorkOS/AuthKit, Convex AuthKit integration                                                                                                                 | Use a real identity provider while keeping workspace decryption trust inside `bowline`.                          |
| Control plane                 | Hosted Convex, Convex schema and functions, Convex Rust client behind `ControlPlaneClient`                                                                 | Live metadata, device approval, compact status, subscriptions, and compare-and-swap refs without operating DBs.  |
| Object bytes                  | Cloudflare R2, Convex R2 component, immutable encrypted packs, encrypted manifests, R2 signed URLs                                                         | Durable encrypted byte storage without one R2 object per source file.                                            |
| Real-root sync                | Native directory materialization, SQLite write log, watcher-triggered daemon sync, reconciliation polling                                                  | Normal tools see normal files at the requested path.                                                             |
| macOS status polish           | Menu bar app, Finder Sync extension badges, UserNotifications, PermissionFlow-style guided setup                                                           | Read-only sync/status affordances without making Finder or a mount backend the source of truth.                  |
| Windows sync polish           | Native directory sync first; Cloud Files API only if it earns its keep                                                                                     | Relevant later, not first demo scope.                                                                            |
| CAS hashing                   | BLAKE3, whole-file hashes first                                                                                                                            | Fast content identity without premature chunking.                                                                |
| Large approved files          | FastCDC-style chunking                                                                                                                                     | Add only for large workspace files where whole-file hashing is wasteful.                                         |
| File watching                 | `notify-rs`, optional Watchman adapter                                                                                                                     | Native events plus reconciliation; watchers must not hydrate the world.                                          |
| Device approval notifications | macOS UserNotifications, Menu Bar Status App, Linux `notify-rust`, freedesktop desktop notifications, CLI/TUI fallback                                     | Approval is status-first; OS notifications are convenience surfaces, not the source of truth.                    |
| Recovery keys                 | Generated word-based keys, CSPRNG entropy, checksum-protected word lists, OS keychain storage for device private keys                                      | Avoid raw key copying without relying on user-chosen phrases or default server-side escrow.                      |
| Setup recipes and receipts    | `.bowlinesetup`, package-manager and toolchain detection, mise/asdf/Volta/direnv/devcontainer import, lockfile-aware install runners                       | Make normal commands work without manual setup on each machine while avoiding broad background script execution. |
| Policy matching               | ripgrep `ignore` crate, glob libraries                                                                                                                     | Reuse matching; build the policy semantics ourselves.                                                            |
| Secrets and env               | Synced encrypted project env store, `.env` import/rematerialization, age/rage-style envelope encryption, SOPS import/export, direnv and mise compatibility | Make project env follow machines, workspaces, and agents without requiring a new launch command.                 |
| Agent integration             | MCP server, local daemon API, OpenHands/devcontainers/Coder integrations                                                                                   | These are consumers of leases, not replacements for leases.                                                      |
| Filesystem tests              | pjdfstest, xfstests, real dev workload tests                                                                                                               | A mount that passes demos but fails editors is not safe enough.                                                  |

## Study but do not center

| Project family                              | Usefulness                                                             | Boundary                                                                           |
| ------------------------------------------- | ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| EdenFS, Sapling, VFS for Git                | Lazy source projection and large-checkout lessons.                     | Source-control checkout systems, not a multi-project workspace substrate.          |
| Kopia, Borg, restic                         | Encrypted dedupe, manifests, object storage, repair ideas.             | Backup repositories, not the live `~/Code` UX.                                     |
| git-annex                                   | File-presence-without-content and large-file ideas.                    | Too Git-shaped as a user-facing workspace model.                                   |
| Syncthing, Mutagen, Unison                  | Sync, reconciliation, and conflict lessons.                            | Bidirectional generic file replication is the wrong source-of-truth model.         |
| JuiceFS, SeaweedFS, rclone mount, s3fs      | Object-backed POSIX, caching, and test discipline.                     | Generic object filesystems do not know Git, env, generated folders, or agents.     |
| jj                                          | Automatic snapshots, change IDs, operation logs, workspace ergonomics. | Design input only; users don't need to understand jj.                              |
| Radicle                                     | Sovereign forge, identity, patches, and replication.                   | Source-control reference only, not part of the sync engine.                        |
| Coder, Codespaces, devcontainers, OpenHands | Runtime and agent execution environments.                              | `bowline` supplies workspace reality and leases; it is not a cloud devbox product. |

## Build ourselves

These are product semantics, not replaceable libraries.

- Workspace graph and namespace authority.
- Source-of-truth split across workspace graph, encrypted CAS, generated/local
  state, secrets, and agent overlays.
- Path conflict rules and project identity model.
- Workspace event model and ref semantics.
- Dev-aware policy compiler with modes like `workspace-sync`,
  `local-regenerate`, `local-cache`, `project-env`, `agent-readable`, and
  `agent-hidden`.
- Scanner and classifier for existing `~/Code` folders.
- Trust/status UX that explains what syncs, what stays local, what is stale,
  what is secret, and what is degraded.
- Materialization semantics: source/config/env/opaque Git bytes become real
  local files quickly; generated/dependency/cache state stays local-regenerate
  or local-cache; large files can remain lazy by policy.
- Setup recipes, setup receipts, and local regeneration so generated folders
  stay local without making users rerun setup manually.
- Agent lease model with fresh base, write scope, hydration budget, inherited
  project env by default, expiry, audit log, output target, and cleanup state.
- Agent-native contract: structured context, primitive tools, prompt recipes,
  event-backed status, capability discovery, and lifecycle verbs.
- Project env sync, conflict handling, and `.env` materialization.
- Conflict model: no last-writer-wins for code, file conflicts keep both, and
  secrets stay versioned and audited.
- Conflict repair TUI that detects installed agent CLIs from `PATH`, creates a
  structured conflict bundle, and always offers a copy-prompt fallback.
- macOS Menu Bar Status App that reports event-backed status, can complete
  pending device approval after explicit confirmation, and leaves repair
  workflow actions to CLI/TUI.
- Device trust flow with pending approval requests, encrypted device grants,
  delegated device approval, generated Recovery Keys, and no default server-side
  key escrow.
- Workspace-wide device trust for approved devices. Agent leases scope agent
  behavior; device grants are not project-scoped by default.
- Remote bootstrap over explicit SSH for installing, registering, approving,
  verifying, and handing off to agents on Linux hosts.
- Merge test scenarios for rename-plus-edit, delete-versus-edit, case-only path
  collisions, structured-text parser failures, `.env` per-key merges, and
  conflict-span edits.

## Build order

The first implementation path proves the product with a real-directory sync
engine on the v1 path. The daemon and status spine come first internally, but
the product proof is not complete until `~/Code` is an ordinary synced directory
that tools can use without wrapper commands.

1. Build `bowline setup --root ~/Code` and `bowline status` first. Existing
   roots are read-only during init; requested missing roots are created as the
   happy path.
2. Detect developer files, generated folders, env files, setup receipts,
   duplicate paths, and explicit local-only projects.
3. Pass the Convex/R2/Rust spike gate: authenticate from Rust, create and
   approve a device, upload an encrypted blob directly to R2, commit metadata
   and advance a ref in Convex, observe the event from a second Rust process,
   download from R2, verify the BLAKE3 hash, and prove stale-ref handling with
   two racing writers.
4. Add workspace refs, device login, device approval requests, delegated device
   approval, encrypted device grants, generated Recovery Keys, and daemon-owned
   real-root sync so a project appears as real files on a second machine.
5. Add the policy compiler, encrypted CAS for workspace-sync files, and local
   dependency regeneration.
6. Add `.bowlinesetup`, setup receipts, and lockfile-backed inference so
   `cd ~/Code/foo && pnpm dev` works on a second machine.
7. Add event-backed status and JSON-first `bowline status`,
   `bowline device approve`, and `bowline status --watch`. The macOS Menu Bar
   Status App can land here as an ambient consumer of that status stream with
   narrow pending device approval, and Linux desktop notifications can use
   `notify-rust` where available.
8. Add `bowline connect` for agent-native Linux host setup over explicit SSH.
9. Add conflict-aside files that sync like any other file, surfaced in
   `bowline status`, with agent CLI detection and copy-prompt fallback for
   reconciliation.
10. Add the agent-native contract: `AgentContextV1`, primitive tools, capability
    discovery, prompt recipes, and lifecycle verbs.
11. Add agent leases with direct project writes by default, optional isolated
    overlays, and inherited project env.
12. Harden real-root sync, distribution, guided permissions, recovery, Finder
    Sync badges, and filesystem workload coverage.
13. Build a merge test corpus that exercises the file identity confidence ladder
    and conflict-span behavior before broader testing.

## Anti-goals

Do not turn `bowline` into any of these:

- a Syncthing or Mutagen fork
- a JuiceFS or object-store filesystem wrapper
- an EdenFS or VFS-for-Git clone
- a Kopia or git-annex UI
- a jj wrapper
- a Radicle competitor
- a Coder or devcontainer manager

Those projects can save implementation time at the edges. They cannot define the
product boundary.
