# bowline decision packet

This packet captures the current product and architecture decisions for the
greenfield "Dropbox for devs" project. The product name is `bowline` going
forward. The docs preserve Theo's original transcript excerpt as the north star,
then record the product thesis, architecture choices, first demo, open risks,
and later source-control pressure tests.

Read the docs in this order:

This list is for the private canonical repo. The generated public source export
contains the client/core source tree plus selected trust-boundary docs; it
intentionally omits private product notes, research, transcripts, reports, and
implementation plans.

1. [Theo north star](./docs/product/theo-north-star.md)
2. [Product thesis](./docs/product/product-thesis.md)
3. [Architecture decisions](./docs/architecture/architecture-decisions.md)
4. [First Theo-grade demo](./docs/product/first-theo-grade-demo.md)
5. [Open questions and risks](./docs/product/open-questions.md)
6. [Inputs and consensus](./docs/research/inputs-and-consensus.md)
7. [Source control and forge boundaries](./docs/architecture/source-control-and-forge-boundaries.md)
8. [Trust contract](./docs/architecture/trust-contract.md)
9. [Untracked file policy](./docs/architecture/untracked-file-policy.md)
10. [OSS composition and build boundaries](./docs/architecture/oss-composition-and-build-boundaries.md)
11. [Work views](./docs/architecture/work-views.md)
12. [Agent-native contract](./docs/architecture/agent-native-contract.md)
13. [Merge discipline](./docs/implementation/merge-discipline.md)

## Public source export

This private repo is canonical. The public source repo is a generated
client/core export for trust and inspectability, not a second source tree.

The export policy lives in [`public-export.json`](./public-export.json).
Maintainers update the public working tree with:

```bash
pnpm export:public -- --target <public-repo>
pnpm check:public-export -- --root <public-repo>
pnpm deploy:public -- --target <public-repo>
```

`deploy:public` creates a generated public source commit locally and only pushes
when `--push` is passed. It does not deploy Convex, Cloudflare, crates.io,
Homebrew, npm packages, app updates, or any other runtime/package channel.

The public repo should use:

```bash
pnpm verify:public
```

`AGENTS.md`, private plans, transcripts, raw research, generated reports, env
files, production deployment wiring, and billing/admin/support internals stay
private.

## Decision summary

Internal motto:

```text
It just works.
```

Build a canonical `~/Code` workspace that appears across local machines and
cloud agents. The tree appears immediately. Projects hydrate on touch. Hot
projects become boring local directories as quickly as possible. The product
understands dev-specific state: ignored/generated folders, non-Git local files,
env and secrets, file state, and agent leases.

V1 uses a real-directory sync model. `~/Code` is an ordinary local directory;
normal shell, editor, Git, package-manager, watcher, Docker, and agent flows
read and write real files. Fresh devices can see structure before every byte is
local, but the product does not depend on a hidden mount, symlink, wrapper
command, or projection backend.

Workspace continuity does not depend on Git ceremony. Local file edits, new
developer files, no-remote folders, and completed agent work follow through
encrypted workspace snapshots by default. Commits, branches, pull requests,
remotes, staging, and `.gitignore` are Git's business, not `bowline` workflow.
`.git/` directories sync as opaque encrypted workspace state. `bowline` does not
merge, repair, publish, or mutate them as Git, and obvious Git lock/temp files
stay local.

`bowline` can run a read-only Git observer for status, freshness, and user
explanation. The observer is advisory: it can report local dirty state, known
remotes, ahead/behind information, and stale bases when that data is available,
but it never drives sync semantics. The workspace snapshot remains the source of
truth for continuity.

WorkOS handles account and organization identity. `bowline` handles device trust
separately. A newly signed-in device or agent host can request workspace access,
but it does not receive decryption keys until an existing authorized device,
authorized local agent session, or generated Recovery Key approves it.
`bowline status`, the TUI, and `bowline approve` show pending requests with a
short matching code. Approved devices receive workspace-wide trust for the
accepted `~/Code` root; `bowline` does not make users manage per-project or
per-path device permissions. Agent leases scope agent work, not device trust.
macOS mirrors them through the Menu Bar Status App and native notifications.
Linux uses `notify-rust` when a desktop notification service exists, and
headless Linux stays fully usable through CLI and TUI status. Native
notification actions are convenience; they are never the only approval path.
`bowline connect <host>` is the agent-native happy path for Linux hosts: it uses
explicit SSH access to install `bowline`, start the daemon, request or complete
device trust through `bowline approve`, verify status, and leave the remote host
as a normal authorized `~/Code` device ready for human or agent work.

The v1 cloud runtime is locked to the smallest stack that preserves "it just
works." The web app and dashboard use TanStack Start on Cloudflare Workers.
WorkOS/AuthKit handles account identity. Hosted Convex owns control-plane
metadata, device requests, encrypted grants, workspace refs, compact events,
status, policies, env records, and agent leases. The Rust daemon talks to Convex
through the Convex Rust client behind a product-shaped `ControlPlaneClient`
boundary. Cloudflare R2 stores immutable encrypted packs and manifests through
the Convex R2 component and direct signed URLs. Source paths never map directly
to R2 object keys. There is no separate Postgres, Railway, Fly, Hyperdrive,
Neon, or cloud Rust service in v1 unless the Convex/R2 spike proves a hard
product limit.

The accepted `~/Code` root is the sync boundary. Monorepos, nested packages,
repos without remotes, and folders that are not repos are all just paths inside
the workspace. If a file under that root is not generated, a dependency, a
cache, or explicitly local-only, it syncs. Ignore files are evidence, not
authority. Git-ignored env, notes, repros, fixtures, and other source-like local
state can still sync when they are not generated, dependency, cache, or
local-only paths. Project path-policy overrides live in `.bowlineignore`: plain
patterns exclude, and `!` patterns explicitly include when a built-in default
was too broad. Including a previously excluded path imports it conservatively:
divergent local copies become a sync conflict, never last-writer-wins. After
import succeeds or resolves, the path follows ordinary workspace rules.
Excluding a previously synced path stops future sync without deleting existing
local copies; cleanup is explicit.

Continuity uses near-continuous snapshots, not last-writer-wins file sync. The
client records local writes, coalesces noisy file events, syncs source-like
state eagerly, and preserves both sides when offline work diverges. Conflict
repair is a first-class status action: the TUI detects installed agent CLIs like
`codex`, `claude`, and `cursor`, offers them as repair choices, and always
offers a copy-prompt fallback.

On macOS, the Menu Bar Status App is a small status icon with a read-only
dropdown for workspace health, pending device approvals, conflicts, degraded
state, and agent activity. It observes the same event-backed status as the CLI
and TUI; it is not a dashboard or control surface. Conflicts stay passive but
visible in the icon, dropdown, CLI, and TUI. Native notifications are reserved
for device approval requests, active work at risk for 60 continuous seconds,
plus review-ready agent work the user started, followed, or is already viewing.
Other review-ready work stays passive in status. The menu bar view is
workspace-wide. `bowline status` and the TUI default to the current project,
with a compact workspace summary when other projects need attention.

Top-level status uses `healthy`, `attention`, and `limited`. `limited` means a
specific path, action, or capability is unavailable; it must say what still
works instead of implying the whole workspace is blocked. Degraded state with no
active work at risk stays passive.

Project env sync is part of the default workspace contract. Existing `.env`,
`.env.local`, and `.env.*` files are imported, synced as encrypted project env
state, and rematerialized where tools expect them. Internally, env is stored as
encrypted per-key records so `.env` changes can merge safely. Source filenames
and profiles are preserved by default: `.env`, `.env.local`, `.env.development`,
and `.env.production` rematerialize as the same files. The same key in multiple
`.env*` files is normal and syncs as separate records. Machines, workspaces, and
agents inherit project env by default; restrictions are explicit opt-in. Env
outside the accepted workspace root, such as shell config, home-directory env,
and machine-global secrets, is not imported unless the user explicitly imports
it. `cd ~/Code/foo && pnpm dev` working on every machine and agent is the bar.

Theo mode means the daily loop uses normal tools, not `bowline` ceremonies.
After install, `bowline` discovers new projects, syncs project env, hydrates
source, regenerates dependencies, applies setup receipts, and honors optional
`.bowlinesetup` recipes for project-specific setup. Setup runs when a project
becomes hot on a machine or agent, not merely because files synced. `bowline`
hands agents a usable workspace without requiring manual hydrate commands,
secret grants, or repo-by-repo configuration for ordinary projects.

First contact is trust-building. `bowline login --root ~/Code` observes an
existing workspace without changing it, then reports concrete state. If the
requested root does not exist, `bowline` can create that empty root or mount the
existing workspace there. It must not silently move projects from other folders
into `~/Code`.

The core product is a developer workspace sync engine, not a Git wrapper,
decentralized forge, or cloud devbox. The internal model is a CAS/ref workspace
graph, but the product stays centered on the code folder. Git remains the user's
tool.

The agent-native machinery stays below the product fold. Humans use normal
tools. Trusted agents work in the real project directory by default, under a
lease that scopes authority, context, budget, env, and audit. Work views give
humans and agents opt-in isolated project views under `~/Code/.work` for risky
or review-before-apply work. Both paths avoid Git ceremony.

## Product sentence

Your code folder, everywhere. `bowline` gives every machine and agent the same
`~/Code` tree, hydrates projects on touch, skips dev junk, carries the right
env, and keeps every agent on a fresh base.

Command examples use `bowline` as the working CLI name.

## CLI contract for agents

The CLI is the local source of truth for agent automation. Agents should start
with:

```bash
bowline contract --json
bowline help --json
bowline status --json
```

`bowline contract --json` lists every command descriptor, JSON output type,
fixture path, protocol version, and bounded-output control.
`bowline schema --json` is the same contract under a discoverable alias. Topic
help works as JSON through both `bowline help <topic> --json` and
`<command> --help --json`, including nested commands such as
`bowline help agent start --json` and `bowline daemon install --help --json`.

Mutations that agents may retry support `--dry-run` for a no-change preview and
`--idempotency-key <key>` for replay-safe non-dry-run execution. The idempotency
request identity includes the current working directory for relative targets and
target-affecting globals such as `--socket`. Recovery commands that read
sensitive stdin reject idempotency keys rather than replaying unvalidated words.
JSON failures use the shared `CommandErrorOutput` envelope on stdout.

Use `bash scripts/agent-use-cli-smoke.sh` for the deterministic local smoke of
the agent CLI contract.

## Project record

- [Docs index](./docs/)
- [Implementation notes](./docs/implementation/merge-discipline.md)
- [Oracle and subagent research](./research/oracle/)
- [Conversation transcript](./transcripts/)
