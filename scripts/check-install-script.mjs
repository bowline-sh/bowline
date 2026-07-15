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
run("bash", ["-n", "scripts/macos/build-release.sh"]);

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
if (!installScript.includes('echo "Next: bowline setup --root ~/Code"')) {
  throw new Error(
    "install.sh must direct fresh installs through authenticated setup",
  );
}
if (macosBuildScript.includes('"$BOWLINE" daemon install')) {
  throw new Error(
    "macOS package postinstall must leave daemon service installation to authenticated setup",
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

run("shellcheck", ["scripts/install.sh", "scripts/smoke-install-headless.sh"]);
