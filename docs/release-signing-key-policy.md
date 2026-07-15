# Release signing key policy

Bowline release roots are signed with an OpenSSH Ed25519 key using SSHSIG
(`ssh-keygen -Y sign`). The installer pins the public key from
`scripts/release-signing-key.pub` and verifies `release-manifest.json` plus
`checksums.txt` before it trusts any version, checksum, or archive.

## Public key

The committed public key is the only trust anchor the installer accepts:

```text
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEMlaBX6Fk+MIYmevcaxgXpsoSdToCQHGmfK0v8Yq3ig bowline-release-2026-07-15
```

`scripts/check-install-script.mjs` fails if the key embedded in
`scripts/install.sh` drifts from that file. Do not add an installer environment
variable or flag that replaces the pinned key at runtime.

The public repo stores the same key at `scripts/release-signing-key.pub`.

## Private key storage

The private key must not be committed to this repo, the public export, release
archives, logs, or issue trackers. Store it in the release operator's secret
store or CI secret manager as a non-interactive OpenSSH private key file.

Publish mode accepts only a key file:

```bash
BOWLINE_RELEASE_SIGNING_KEY_FILE=/path/to/release-key pnpm release:assets -- --version 0.1.3 --publish
```

Raw private key material in an environment variable is not accepted for publish.
This keeps release signing out of shell history, process environments, and
accidental logs.

## Key generation

Generate a new release key with:

```bash
ssh-keygen -t ed25519 -C bowline-release-YYYY-MM-DD -f bowline-release
```

Commit only `bowline-release.pub` as `scripts/release-signing-key.pub`, then
update `scripts/install.sh` so `RELEASE_SIGNING_PUBKEY` contains exactly the
same line. Run:

```bash
pnpm check:install-script
pnpm release:authenticity-smoke
```

## Signing behavior

`scripts/release-assets.mjs` signs:

- `checksums.txt` as `checksums.txt.sig`
- `release-manifest.json` as `release-manifest.json.sig`

Publish mode requires `BOWLINE_RELEASE_SIGNING_KEY_FILE` and fails before upload
when no key file is configured. Dry-run mode can run without a key and will skip
`.sig` emission so local release checks do not need production secret access.

The signature namespace and allowed signer identity are both `bowline-release`.
The signed bytes are the exact file bytes on disk after each root is finalized.

Manual verification uses the OpenSSH allowed-signers format:

```bash
cat > allowed-signers <<'EOF'
bowline-release ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEMlaBX6Fk+MIYmevcaxgXpsoSdToCQHGmfK0v8Yq3ig bowline-release-2026-07-15
EOF

ssh-keygen -Y verify -f allowed-signers -I bowline-release -n bowline-release \
  -s release-manifest.json.sig < release-manifest.json
ssh-keygen -Y verify -f allowed-signers -I bowline-release -n bowline-release \
  -s checksums.txt.sig < checksums.txt
```

## Rotation

Rotation creates a new public trust anchor and intentionally invalidates
releases signed only by the old key under new installers.

1. Generate a new Ed25519 keypair.
2. Store the private key in the release operator secret store.
3. Replace `scripts/release-signing-key.pub`.
4. Replace the embedded `RELEASE_SIGNING_PUBKEY` in `scripts/install.sh`.
5. Re-sign current release roots with the new private key.
6. Run `pnpm check:install-script` and `pnpm release:authenticity-smoke`.
7. Publish a new installer and release asset set together.

## Residual risk

This policy authenticates release roots and binaries after the installer script
starts running. The `curl | sh` script fetch itself is still protected by TLS to
`install.bowline.sh`, not by this pinned key. Publishing the installer's own
checksum or signature through an out-of-band channel is a separate follow-up.
