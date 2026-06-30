# Source control and forge boundaries

This document captures the later pressure tests against Theo's source-control
thread, his jj comments, and Radicle. The conclusion is that `bowline` stays out
of source control. It syncs workspace files and leaves Git, jj, GitHub, and
Radicle to the user's existing tools.

## Layer map

The related ideas live at different layers of the same agent-era development
problem.

```text
Dropbox for devs:
  Workspace presence across machines and agents.

Theo's source-control thread:
  Useful context, but outside the sync engine.

jj:
  A strong model reference for snapshots, change IDs, operation logs, and
  less ceremonial local work.

Radicle:
  A sovereign peer-to-peer forge and collaboration network built on Git.
```

`bowline` sits closest to the first layer. It must not become the others.

## Decision: Keep the product centered on `~/Code`

The user-facing product remains the canonical code folder.

The first promise is still:

```text
Your ~/Code tree exists everywhere.
Machines and agents see the same workspace.
Projects hydrate when touched.
Env, secrets, file state, and policy follow safely.
```

This keeps the product aligned with the original Dropbox-for-devs transcript.
Source-control tools remain outside the product boundary.

## Decision: Source control stays outside `bowline`

Git support means not breaking Git projects by syncing the workspace files they
contain. It does not mean Git integration. `bowline` must not mutate Git, edit
`.gitignore`, stage files, create commits, create branches, manage remotes,
publish pull requests, repair Git state, or export to a forge.

`.git/` directories still sync as opaque encrypted workspace state. This is file
sync, not Git integration. Obvious `.git/` lock and temp files stay local, and
divergent `.git/` files create normal sync conflicts.

A read-only Git observer is allowed for status, freshness, and user explanation.
It can inspect local repository metadata and local remote-tracking state to
report dirty state, known remotes, ahead/behind state, and stale bases. It must
not fetch, write, repair, merge, publish, or decide sync semantics. `.git/`
remains ordinary opaque workspace bytes to the sync engine.

Internally, `bowline` needs its own workspace graph:

```text
workspace namespace
file snapshots
machine overlays
agent overlays
policy graph
hydration graph
real-directory sync engine
agent API
```

This is the distinction that keeps `bowline` a sync engine.

## Decision: Workspace state does not require Git publish

Git cleanliness is not the product contract. A directory with local edits, new
developer files, or no remote still has real work that must follow the user.

`bowline` captures that state in its own workspace graph:

- source files become encrypted workspace state
- `.git/` directories become opaque encrypted workspace state
- new developer files become encrypted workspace state
- project env becomes encrypted project env state
- generated folders, dependency folders, and caches stay local or regenerate

The user can publish later with Git or any other tool. Until then, the work
still appears in `~/Code` on the next machine or agent runtime.

## Decision: Keep source control below the product fold

Source control is not substrate for the sync engine. Lead with "your code folder
exists everywhere."

The early external promise is:

```text
bowline syncs your code folder and gives machines and agents the same
workspace files.
```

The internal workspace graph makes that promise safer and more agent-native. It
does not turn into source control.

## Decision: Use jj as a model, not a product dependency

Theo's jj comments are useful product context:

- Work is captured continuously.
- Change identity is separate from branch naming.
- Operation history gives users undo and recovery.
- Workspaces feel less painful when local work is captured continuously.
- Undo and recovery matter.
- Users should not learn new source-control concepts to sync files.

Useful ideas stop at local snapshots, overlays, and recovery.

Branch, commit, and pull request decisions stay outside `bowline`. The default
continuation path is the bowline snapshot, because that is what makes local work
resume without a remote.

## Decision: Do not build Radicle

Radicle is adjacent, but it is not the product. Radicle is a sovereign
peer-to-peer forge built on Git. It focuses on repository replication, identity,
patches, issues, and collaboration without a central GitHub-style host.

`bowline` focuses on workspace reality:

- the same `~/Code` tree across machines and agents
- lazy hydration
- dev-aware generated-file and cache policy
- env and secrets parity
- file state
- agent leases
- index-backed code exploration before hydration
- API-native file access for agents

Radicle can remain a design reference. It is not part of the sync engine.

## Updated product shape

The sharper product sentence is:

```text
Dropbox-like workspace presence,
for code folders and coding agents.
```

The longer version is:

```text
bowline is a canonical developer workspace sync engine,
projected as ~/Code for humans,
exposed as APIs for agents,
and not built around commits, branches, worktrees, or a forge.
```

That shape connects Theo's Dropbox-for-devs idea, his source-control thread, and
his jj enthusiasm without collapsing `bowline` into source control.

## Explicit non-goals

These ideas are not the product center:

- A decentralized GitHub.
- A Radicle competitor.
- A Git wrapper.
- A jj wrapper.
- A P2P repo replication network.
- A source-control CLI without the `~/Code` workspace sync engine.

They can exist outside `bowline`, but they are not part of the sync engine.
substrate.
