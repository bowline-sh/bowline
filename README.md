# Bowline

Bowline keeps the same developer workspace available across your machines and
agents. Use `~/Code` like a normal local folder; Bowline handles device trust,
workspace sync, generated-file policy, and agent work isolation underneath.

This repository contains Bowline's public client and runtime source. It is a
generated export for release builds, audits, and local contribution against the
client boundary. Private product notes, hosted deployment wiring, credentials,
research packets, and unreleased plans are intentionally not part of this repo.

## Install

On Apple Silicon macOS:

```bash
brew tap bowline-sh/homebrew-tap
brew trust --formula bowline-sh/tap/bowline
brew install bowline-sh/tap/bowline
```

On Linux, download the `bowline-x86_64-unknown-linux-gnu.tar.xz` archive from
the latest GitHub release, unpack it, and put `bowline` and `bowline-daemon` on
your `PATH`.

Check the install:

```bash
bowline --version
bowline-daemon --version
```

## First Machine

Create or adopt your workspace:

```bash
bowline login --root ~/Code
bowline status
```

`bowline login` opens the account flow, creates the workspace if needed, and
trusts the first device. `bowline status` shows sync state, pending device
approvals, agent work, and recovery actions.

## Second Machine

Install Bowline on the second machine, then run:

```bash
bowline login --root ~/Code
bowline status
```

Approve the new device from an already trusted machine when prompted. After
approval, edits under `~/Code` sync through the hosted control plane and object
store. Generated folders such as `node_modules` stay local by default.

## Agent Work

Agents should use leases instead of writing directly into the live workspace:

```bash
bowline agent lease create ~/Code/my-project --task "describe the work"
bowline work list
```

Leases give agents a scoped workspace, hydration budget, freshness checks, and a
review path before changes land back in the main project.

## Build From Source

```bash
pnpm install --frozen-lockfile
pnpm verify:public
cargo build --release -p bowline -p bowline-daemon
```

The release binaries are:

- `target/release/bowline`
- `target/release/bowline-daemon`

## Repository Boundary

The public repo is generated from Bowline's private canonical repo. Do not add
private deployment configuration, raw env files, internal plans, transcripts, or
research material here. Public source changes should be made in the canonical
repo and exported.
