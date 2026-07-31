#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";

function run(command, args) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: "inherit",
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed`);
  }
}

run("sh", ["-n", "scripts/install.sh"]);
run("sh", ["-n", "scripts/package-release-binary.sh"]);
run("sh", ["-n", "scripts/smoke-install-headless.sh"]);
run("sh", ["-n", "scripts/smoke-install-upgrade.sh"]);
run("bash", ["-n", "scripts/macos/build-release.sh"]);
run("env", [
  "PATH=/usr/bin:/bin",
  "CARGO_BIN=",
  "scripts/macos/build-release.sh",
  "--help",
]);

const installScript = readFileSync("scripts/install.sh", "utf8");
const macosBuildScript = readFileSync("scripts/macos/build-release.sh", "utf8");
const headlessSmoke = readFileSync("scripts/smoke-install-headless.sh", "utf8");
const releaseKey = readFileSync(
  "scripts/release-signing-key.pub",
  "utf8",
).trim();

const embeddedKeyMatch = installScript.match(
  /^RELEASE_SIGNING_PUBKEY="([^"]+)"$/mu,
);
if (!embeddedKeyMatch) {
  throw new Error("install.sh must embed RELEASE_SIGNING_PUBKEY");
}
if (embeddedKeyMatch[1] !== releaseKey) {
  throw new Error(
    "pinned release signing key drift: scripts/install.sh must match scripts/release-signing-key.pub",
  );
}

run("ssh-keygen", ["-l", "-f", "scripts/release-signing-key.pub"]);

const forbiddenKeyOverrides = [
  "BOWLINE_RELEASE_PUBKEY",
  "BOWLINE_RELEASE_SIGNING_PUBKEY",
  "BOWLINE_RELEASE_PUBKEY_FILE",
];
for (const token of forbiddenKeyOverrides) {
  if (installScript.includes(token)) {
    throw new Error(`install.sh must not allow runtime key override: ${token}`);
  }
}

const requiredOrder = [
  "resolve_release_base\n",
  'download "$RELEASE_BASE/checksums.txt" "$TMPDIR/checksums.txt"\n',
  'download "$RELEASE_BASE/checksums.txt.sig" "$TMPDIR/checksums.txt.sig"\n',
  'verify_manifest_bound_file checksums "$TMPDIR/checksums.txt"\n',
  'verify_manifest_bound_file checksums_sig "$TMPDIR/checksums.txt.sig"\n',
  'verify_signature "$TMPDIR/checksums.txt" "$TMPDIR/checksums.txt.sig"\n',
  'if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then\n',
];
let cursor = 0;
for (const marker of requiredOrder) {
  const index = installScript.indexOf(marker, cursor);
  if (index === -1) {
    throw new Error(
      `install.sh signature flow is missing or reordered near: ${marker.trim()}`,
    );
  }
  cursor = index + marker.length;
}

if (
  installScript.includes("daemon install") ||
  installScript.includes("install_daemon")
) {
  throw new Error(
    "install.sh must leave daemon service installation to authenticated setup",
  );
}
if (!installScript.includes('"$INSTALL_DIR/bowline" daemon restart')) {
  throw new Error("install.sh must restart the daemon binary it just replaced");
}
for (const marker of [
  'CURRENT_LINK="$INSTALL_DIR/.bowline-current"',
  "validate_binaries",
  "restart_and_verify_daemon",
  "rollback_switch",
  'INSTALL_RESULT="installed-and-healthy"',
  'INSTALL_RESULT="installed-on-disk-restart-required"',
  "BOWLINE_INSTALL_RESULT=%s",
]) {
  if (!installScript.includes(marker)) {
    throw new Error(
      `install.sh transactional upgrade flow is missing: ${marker}`,
    );
  }
}
if (
  installScript.includes(
    'install -m 0755 "$TMPDIR/cli/bowline" "$INSTALL_DIR/bowline"',
  ) ||
  installScript.includes('rm -rf "$APP_DIR/Bowline.app"')
) {
  throw new Error("install.sh must not replace live release members in place");
}
if (!installScript.includes("quit_macos_app")) {
  throw new Error(
    "install.sh must quit the running app bundle before replacing it",
  );
}
if (
  !installScript.includes('NEXT_COMMAND="bowline setup --root ~/Code"') ||
  !installScript.includes(
    'NEXT_COMMAND="$INSTALL_DIR/bowline setup --root ~/Code"',
  ) ||
  !installScript.includes('echo "Next: $NEXT_COMMAND"')
) {
  throw new Error(
    "install.sh must direct fresh installs through authenticated setup with a command the user can actually run",
  );
}
if (macosBuildScript.includes('"$BOWLINE" daemon install')) {
  throw new Error(
    "macOS package postinstall must leave daemon service installation to authenticated setup",
  );
}
if (
  !macosBuildScript.includes(
    'CARGO_BIN="${CARGO_BIN:-$(command -v cargo || true)}"',
  ) ||
  !macosBuildScript.includes('"$CARGO_BIN" build')
) {
  throw new Error(
    "macOS release build must resolve and invoke a Cargo executable directly",
  );
}
if (
  !macosBuildScript.includes(
    'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"',
  ) ||
  !macosBuildScript.includes(
    'cp "$CARGO_TARGET_DIR/release/bowline" "$APP_BUNDLE/Contents/Resources/bin/bowline"',
  ) ||
  !macosBuildScript.includes(
    'cp "$CARGO_TARGET_DIR/release/bowline-daemon" "$APP_BUNDLE/Contents/Resources/bin/bowline-daemon"',
  )
) {
  throw new Error(
    "macOS release bundle must copy Rust binaries from the active Cargo target directory",
  );
}
if (macosBuildScript.includes("$ROOT/target/release/bowline")) {
  throw new Error(
    "macOS release build must not copy from a hard-coded target tree",
  );
}
if (
  !macosBuildScript.includes(
    'VERSION="${BOWLINE_MACOS_VERSION:-$WORKSPACE_VERSION}"',
  )
) {
  throw new Error(
    "macOS release version must default to the Rust workspace version",
  );
}

for (const platformCase of ["Linux:x86_64)", "Linux:aarch64 | Linux:arm64)"]) {
  if (!installScript.includes(platformCase)) {
    throw new Error(`install.sh is missing headless target: ${platformCase}`);
  }
}

if (!headlessSmoke.includes('"$TMPDIR/bin/bowline" version')) {
  throw new Error(
    "headless smoke must exercise the canonical CLI version command",
  );
}
if (headlessSmoke.includes('"$TMPDIR/bin/bowline" --version')) {
  throw new Error("headless smoke must not use the unsupported --version flag");
}

run("shellcheck", [
  "scripts/install.sh",
  "scripts/smoke-install-headless.sh",
  "scripts/smoke-install-upgrade.sh",
]);
