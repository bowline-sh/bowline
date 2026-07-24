<p align="center">
  <picture>
    <source
      media="(prefers-color-scheme: dark)"
      srcset="assets/bowline-icon-dark.png"
    />
    <img src="assets/bowline-icon-light.png" alt="Bowline" width="96" />
  </picture>
</p>

# Bowline

Bowline keeps one developer workspace available across all your machines and
coding agents. You work in `~/Code` like a normal local folder, and Bowline
handles device trust, workspace sync, generated-file policy, and agent work
isolation underneath.

This repository holds Bowline's public client and runtime source. It's a
generated export from a private canonical repo, meant for release builds,
audits, and contributions to the public client. It leaves out private product
notes, hosted deployment wiring, credentials, research packets, and unreleased
plans by design.

## Install

On Apple Silicon macOS and Linux x86_64:

```bash
curl -fsSL https://install.bowline.sh | sh
```

On macOS, this installs `Bowline.app`, `bowline`, and `bowline-daemon`. On
Linux, it installs `bowline` and `bowline-daemon` into `~/.local/bin`.

For CLI-only installs on macOS:

```bash
curl -fsSL https://install.bowline.sh | sh -s -- --cli-only
```

If you prefer Homebrew:

```bash
brew install bowline-sh/tap/bowline
```

Verify the install:

```bash
bowline version
bowline-daemon --version
```

## First machine

Create or adopt your workspace:

```bash
bowline setup --root ~/Code
bowline status
```

`bowline setup` opens the account flow when needed, creates or adopts the
workspace root, and trusts the first device. `bowline status` shows sync state,
pending device approvals, agent work, and recovery actions.

## Second machine

Install Bowline on the second machine, then run:

```bash
bowline setup --root ~/Code
bowline status
```

When prompted, approve the new device from a machine you already trust. After
approval, edits under `~/Code` sync through the hosted control plane and object
store. Generated folders such as `node_modules` stay local by default.

## Agent work

Agents work in the same synced `~/Code` directories you do. When you want to
review before changes land, create an isolated work view and point your agent at
it:

```bash
bowline work create ~/Code/my-project review-run
bowline work list
```

A work view is a clean, cd-able directory with the project's env and a fresh
base. The agent's edits stay isolated until you review and accept them with
`bowline work review` and `bowline work accept`, or drop them with
`bowline work discard`.

## Build from source

You need pnpm and a Rust toolchain installed. Then build the release binaries:

```bash
pnpm install --frozen-lockfile
pnpm verify
cargo build --release -p bowline -p bowline-daemon
```

The release binaries are:

- `target/release/bowline`
- `target/release/bowline-daemon`

## Repository boundary

The public repo is a generated export of Bowline's private canonical repo. Don't
add private deployment config, raw env files, internal plans, transcripts, or
research material here. Make public source changes in the canonical repo, then
export them.
