# Trust contract

This document states the behavior `bowline` must preserve before users can trust
it with their real `~/Code` folder. These are product guarantees, not
implementation preferences.

## Non-negotiable rules

The product must never trade developer trust for sync convenience. If the system
cannot prove an action is safe, it must stop and explain the state.

- Never silently delete unknown local files.
- Never mutate an existing workspace during first setup. `bowline setup` can
  observe, classify, index, and report; it cannot write project files, mutate
  Git, import secrets, run setup, hydrate content, or sync portable state.
- Never silently move projects from other roots into `~/Code`. Creating a
  requested missing root is safe; moving existing work requires explicit intent.
- Never make the happy path depend on `bowline` commands after install.
- Never require a Git remote, clean working tree, commit, branch, or pull
  request before work can follow the user.
- Never mutate Git, edit `.gitignore`, stage files, create commits or branches,
  manage remotes, repair Git state, or publish to a forge. Git is the user's
  tool.
- Never make Git status authoritative for sync. A read-only Git observer can
  explain freshness, dirty state, known remotes, ahead/behind state, and stale
  bases, but it must not fetch, write, repair, merge, publish, or decide sync
  semantics.
- Never merge, repair, or publish `.git/` as Git. Sync `.git/` as opaque
  encrypted workspace state, excluding only the named machine-local set (index,
  locks, reflogs, operation scratch, temp objects) recorded in architecture
  decision 2; never delete local copies of excluded paths.
- The sole `.git` content `bowline` rewrites is the machine-local absolute path
  inside a linked worktree's gitlink and its `gitdir`/`commondir` admin files,
  and only to move that path in or out of the synced workspace root; never
  delete the local copy, never touch any other git content.
- Never use repo, package, or monorepo boundaries to stop default sync inside
  the accepted workspace root.
- Never treat an ignore rule as proof that real workspace state must stay local.
  Ignore rules are evidence for classification, not final sync authority.
- Never auto-write `.bowlineignore` during init or scan. Create path-policy
  files only when the user explicitly changes policy.
- Never use last-writer-wins when a previously excluded path is included. If
  local copies differ across machines, preserve them as a sync conflict.
- Never delete existing local copies when a previously synced path becomes
  excluded. Cleanup requires explicit user action.
- Never require manual copying of project env between machines, workspaces, or
  agents.
- Never import env outside the accepted workspace root, such as shell config,
  home-directory env, or machine-global secrets, unless the user explicitly
  imports it.
- Never require manual hydrate or setup commands before normal project commands
  can run, unless the product reports a concrete blocker.
- Never store or transmit project env unencrypted remotely; local `.env`
  materialization is expected when the project needs it.
- Never use Git-aware merge or last-writer-wins for divergent `.git/` files;
  preserve them as sync conflicts.
- Never release workspace decrypt keys to a new device solely because account
  authentication or MFA succeeded.
- Never treat Phase 4 account or device metadata as decrypt authority. Records
  can exist before the Phase 5 grant and recovery flows, but they cannot unlock
  workspace data.
- Never require a human click when an authorized local agent session has
  explicit user intent to approve a remote host it is bootstrapping.
- Never treat SSH reachability as workspace trust. SSH can carry bootstrap
  commands; an authorized device or Recovery Key still creates the encrypted
  grant.
- Never send device private keys, Recovery Key words, raw workspace keys, or
  decrypted grants to Convex, R2, WorkOS, SSH arguments, logs, events, or JSON
  output.
- Never create default per-project, per-path, or lease-scoped device grants.
  Trusted devices get the accepted workspace root; agent leases scope agent
  behavior.
- Never make native notifications the only path for device approval.
  `bowline status`, the TUI, and `bowline device approve` must show pending
  approvals.
- Never accept user-chosen passphrases as workspace recovery material. Recovery
  Keys are generated word-based keys.
- Never provide default server-side recovery for encrypted workspace data
  without an authorized device or Recovery Key.
- Never use last-writer-wins for code.
- Never auto-apply an agent conflict resolution to the live project.
- Never inject conflict markers into live project files automatically.
- Never block a whole text file when only a smaller conflict span is unsafe.
- Never collapse delete-versus-edit into an automatic delete or edit.
- Never claim a workspace is healthy while degraded or offline.
- Never mark a whole workspace or project as blocked when only a specific path,
  action, or capability is limited.
- Never publish agent work without an explicit review or publish path.
- Never confuse workspace continuity with source-control publishing. Syncing
  files is the product; publishing is outside `bowline`.
- Never restrict project env for agents or workspaces unless the user or org
  explicitly opted into that restriction.
- Never hide local-only, blocked, dirty, untracked, or no-remote state from
  `bowline status`.
- Never make the Menu Bar Status App a required control surface or repair
  workflow.
- Never make the web dashboard, menu bar app, or native notifications the
  required repair or decision surface. CLI/TUI must remain complete, including
  headless operation.
- Never send native notifications for conflict creation, healthy sync,
  hydration, indexing, or agent progress. Device approval requests are the
  narrow trust exception.

## Status obligations

Status output is part of the trust boundary. A user must be able to inspect the
workspace and understand what follows them, what stays local, and what needs
attention.

`bowline status`, `bowline status --json`, and the TUI default to the current
project when run inside one. They include a compact workspace summary when
another project needs attention. The Menu Bar Status App is workspace-wide by
default.

Top-level status levels are `healthy`, `attention`, and `limited`. `limited`
must name the specific path, action, or capability that cannot proceed and what
still works. Use blocking language only for those specific details. Except for
pending device approval requests, native notifications require active work at
risk: local edits that cannot sync out, required hydration or materialization
that cannot proceed, a user-invoked `bowline` operation that is stuck, or an
agent lease that cannot safely continue for 60 continuous seconds. Degraded
state with no active work at risk stays passive.

`bowline status` must show:

- workspace health
- sync and hydration state
- setup receipt and dependency regeneration state
- workspace freshness
- pending device approval requests, including device name, request age, and
  matching code
- device trust and Recovery Key state when they affect access or recovery
- env sync, access, profile, and materialization state for values `bowline` has
  seen
- generated-folder policy
- workspace continuity state for local edits, new files, excluded paths, and
  no-remote folders
- conflict records with base, local, remote, and resolution state
- explicit local-only and blocked paths
- agent leases and stale bases
- degraded watcher, sync, or network state
- event watermarks, such as last scan, last event, event lag, sync state,
  watcher state, and network state when they affect trust
- suggested next safe actions when the workspace needs attention

The Menu Bar Status App consumes this same status. It can show a compact icon
and ambient dropdown for workspace health, pending device approvals, conflicts,
degraded state, sync and hydration state, and agent activity. It may approve or
deny a pending device only after explicit inline confirmation, using the same
`bowline device approve --request <id> --yes --json` and
`bowline device deny --request <id> --json` trust paths as the CLI. CLI and TUI
remain the durable surfaces for all other actions and repair through
`bowline status --json`, `bowline tui [path]`, and
`bowline resolve <project> --tui`. Headless hosts must be able to complete the
same decisions through CLI prompts, JSON output, and copy-prompt repair.
Conflicts stay passive but visible; the app must not steal focus or auto-launch
repair. Native notifications are allowed only for pending device approvals,
blocking degraded state with active work at risk, or review-ready agent work the
user started, followed, or is already viewing. Other review-ready agent work
stays passive in status.

## Agent obligations

Agents need stronger boundaries than human machines because they can read and
write quickly, request broad context, and operate from stale assumptions.

Every agent task must run through a lease with:

- a fresh workspace snapshot
- a write target: direct project by default, isolated work-view overlay when
  requested
- inherited project env by default
- explicit env restrictions only when configured
- an RFC3339 expiry that is later than creation and capped by the supported
  maximum lease duration
- an audit trail
- an output target, such as a workspace continuation snapshot or patch bundle

The default output target is a workspace continuation snapshot. If the lease
finishes without conflicting with newer workspace work, that snapshot becomes
the project state that follows the user to the next machine. Publishing is
outside `bowline`.

Agents inspect the synced project tree with their own tools. Workspaces
materialize by default, so leases don't impose a hydration budget. The sync and
materialization paths must still enforce bounded scope, path validation,
resource ceilings, and fail-closed handling for malformed or expired lease
metadata.

Agent context is part of the trust boundary. `AgentContextV1` must include the
fresh workspace snapshot, lease scope, env metadata without secret values,
policy version, index freshness, status snapshot, and hard instructions that
enforce these rules.

## Recovery obligations

Infrastructure users forgive rough edges when recovery is clear. They do not
forgive silent data loss.

`bowline` must provide:

- local snapshots before risky operations
- explicit conflict records
- preserved copies for file conflicts
- conflict bundles for repair tools
- stable local active views while conflicts are unresolved
- resolution overlays that require user acceptance
- encrypted device grants
- delegated device approval from authorized local sessions
- remote bootstrap over explicit SSH
- generated Recovery Key creation, verification, rotation, and recovery flow
- device revocation
- uninstall instructions that leave projects readable
- cache cleanup without source deletion
- audit logs for device approval, recovery, secret, and agent actions
- degraded-mode warnings when offline or partially mounted
- lifecycle operations for archive, delete, forget-local, dehydrate, revoke,
  cleanup, restore, and purge
- event history for automatic hydration, setup, env, policy, lease, overlay,
  publish, conflict, and recovery work

The product can be incomplete during early builds. It cannot be vague about
state, ownership, or recovery.

## Implemented local key custody behavior

The Phase 5 implementation uses a `DeviceKeyStore` boundary for local custody.
Production uses the OS keychain through the Rust `keyring` crate where
available. Tests use a deterministic fake keychain. Headless Linux bootstrap can
use an explicit server-local fallback outside the synced workspace:

```text
$XDG_STATE_HOME/bowline/secrets.v1
~/.local/state/bowline/secrets.v1
```

The fallback file is created for the remote OS user only, with parent directory
mode `0700` and file mode `0600`. It is never placed under `~/Code`, never
synced, and is reported as `secretStore: server-local`. This is acceptable for
the explicit SSH bootstrap path, but it is not equivalent to a hardware-backed
desktop keychain.

Passive commands must not probe the desktop keychain by default. Commands such
as `bowline setup`, `bowline status`, contract tests, and smoke scripts may
decorate their output with auth or trust state only when a non-interactive
secret store is explicitly configured through `BOWLINE_SECRET_STORE_PATH` or
`BOWLINE_SECRET_STORE=server-local`. Development and CI subprocesses should set
a temporary `BOWLINE_SECRET_STORE_PATH` instead of touching the user login
keychain. `BOWLINE_ALLOW_KEYCHAIN_PROBE=1` is the explicit escape hatch for
passive commands that intentionally want to read existing desktop keychain
state.

Recovery uses generated 24-word BIP39 keys. The words decrypt an age
passphrase-encrypted recovery envelope locally, then bowline creates the same
pending request and encrypted grant audit trail used by normal device approval.
Convex stores encrypted envelopes and grant ciphertext, not Recovery Key words
or plaintext workspace keys.
