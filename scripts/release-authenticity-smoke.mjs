#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  accessSync,
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
  constants,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import {
  releaseAllowedSignersLine,
  releaseSigningIdentity,
  signReleaseFile,
  verifyReleaseFile,
} from "./release-signing.mjs";

const sourceRoot = process.cwd();
const version = "0.0.1-smoke";
const target =
  process.platform === "darwin"
    ? "aarch64-apple-darwin"
    : "x86_64-unknown-linux-gnu";

function run(command, args, options = {}) {
  return spawnSync(command, args, {
    cwd: options.cwd ?? sourceRoot,
    encoding: "utf8",
    env: options.env ?? process.env,
    input: options.input,
    stdio: options.input
      ? ["pipe", "pipe", "pipe"]
      : ["ignore", "pipe", "pipe"],
  });
}

function mustRun(command, args, options = {}) {
  const result = run(command, args, options);
  if (result.status !== 0) {
    throw new Error(
      `${command} ${args.join(" ")} failed\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  return result;
}

function sha256File(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function assertSshsigCli() {
  const workspace = mkdtempSync(path.join(tmpdir(), "bowline-sshsig-"));
  try {
    const key = path.join(workspace, "key");
    const message = path.join(workspace, "message");
    const allowed = path.join(workspace, "allowed-signers");
    mustRun("ssh-keygen", [
      "-q",
      "-t",
      "ed25519",
      "-N",
      "",
      "-C",
      releaseSigningIdentity,
      "-f",
      key,
    ]);
    writeFileSync(message, "hello");
    signReleaseFile(message, key, { capture: true });
    writeFileSync(
      allowed,
      releaseAllowedSignersLine(readFileSync(`${key}.pub`, "utf8")),
    );
    verifyReleaseFile(message, `${message}.sig`, allowed, { cwd: workspace });
  } finally {
    rmSync(workspace, { recursive: true, force: true });
  }
}

function writeArchive(workspace, archive) {
  const stage = path.join(workspace, "stage");
  mkdirSync(stage, { recursive: true });
  writeFileSync(path.join(stage, "bowline"), "#!/bin/sh\nexit 0\n");
  writeFileSync(path.join(stage, "bowline-daemon"), "#!/bin/sh\nexit 0\n");
  chmodSync(path.join(stage, "bowline"), 0o755);
  chmodSync(path.join(stage, "bowline-daemon"), 0o755);
  mustRun("tar", ["-C", stage, "-czf", archive, "bowline", "bowline-daemon"]);
}

function writeManifest(file, releaseDir, archiveName, archive, checksums) {
  writeFileSync(
    file,
    `${JSON.stringify(
      {
        version,
        publishedAt: "2026-07-05T00:00:00.000Z",
        urgency: "normal",
        artifacts: {
          checksums: {
            url: `file://${releaseDir}/checksums.txt`,
            sha256: sha256File(checksums),
          },
          checksums_sig: {
            url: `file://${releaseDir}/checksums.txt.sig`,
            sha256: sha256File(`${checksums}.sig`),
          },
          cli: {
            url: `file://${releaseDir}/${archiveName}`,
            sha256: sha256File(archive),
          },
        },
      },
      null,
      2,
    )}\n`,
  );
}

function writeFixture(workspace, keyFile, pubkey, fixtureTarget = target) {
  const root = path.join(workspace, "release");
  const releaseDir = path.join(root, "releases", `v${version}`);
  const latestDir = path.join(root, "releases", "latest");
  mkdirSync(releaseDir, { recursive: true });
  mkdirSync(latestDir, { recursive: true });

  const archiveName = `bowline-${fixtureTarget}.tar.gz`;
  const archive = path.join(releaseDir, archiveName);
  writeArchive(workspace, archive);

  const checksums = path.join(releaseDir, "checksums.txt");
  writeFileSync(checksums, `${sha256File(archive)}  ${archiveName}\n`);
  signReleaseFile(checksums, keyFile, { capture: true });

  const manifest = path.join(releaseDir, "release-manifest.json");
  writeManifest(manifest, releaseDir, archiveName, archive, checksums);
  signReleaseFile(manifest, keyFile, { capture: true });

  for (const name of [
    archiveName,
    "checksums.txt",
    "checksums.txt.sig",
    "release-manifest.json",
    "release-manifest.json.sig",
  ]) {
    copyFileSync(path.join(releaseDir, name), path.join(latestDir, name));
  }
  copyFileSync(manifest, path.join(root, "release-manifest.json"));
  copyFileSync(`${manifest}.sig`, path.join(root, "release-manifest.json.sig"));

  const installer = path.join(workspace, "install.sh");
  const sourceInstaller = readFileSync(
    path.join(sourceRoot, "scripts", "install.sh"),
    "utf8",
  );
  const patchedInstaller = sourceInstaller.replace(
    /^RELEASE_SIGNING_PUBKEY="[^"]+"$/mu,
    `RELEASE_SIGNING_PUBKEY="${pubkey.trim()}"`,
  );
  if (patchedInstaller === sourceInstaller) {
    throw new Error("failed to patch installer key in smoke copy");
  }
  writeFileSync(installer, patchedInstaller);
  chmodSync(installer, 0o755);

  return { archive, checksums, installer, keyFile, manifest, releaseDir, root };
}

function makeFixture(fixtureTarget = target) {
  const workspace = mkdtempSync(path.join(tmpdir(), "bowline-release-auth-"));
  const keyFile = path.join(workspace, "release-key");
  mustRun("ssh-keygen", [
    "-q",
    "-t",
    "ed25519",
    "-N",
    "",
    "-C",
    releaseSigningIdentity,
    "-f",
    keyFile,
  ]);
  const pubkey = readFileSync(`${keyFile}.pub`, "utf8");
  return {
    fixture: writeFixture(workspace, keyFile, pubkey, fixtureTarget),
    workspace,
  };
}

function toolPath(name) {
  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!directory) continue;
    const candidate = path.join(directory, name);
    try {
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Keep searching the PATH.
    }
  }
  return null;
}

function constrainedPath(workspace) {
  const bin = path.join(workspace, "constrained-bin");
  mkdirSync(bin, { recursive: true });
  for (const command of [
    "awk",
    "basename",
    "cat",
    "chmod",
    "cp",
    "curl",
    "grep",
    "install",
    "mkdir",
    "mktemp",
    "rm",
    "sed",
    "sh",
    "tar",
    "uname",
  ]) {
    const resolved = toolPath(command);
    if (resolved && existsSync(resolved)) {
      symlinkSync(resolved, path.join(bin, command));
    }
  }
  return bin;
}

function pathWithUname(workspace, system, machine) {
  const bin = path.join(workspace, "platform-bin");
  mkdirSync(bin, { recursive: true });
  const uname = path.join(bin, "uname");
  writeFileSync(
    uname,
    `#!/bin/sh\ncase "$1" in\n  -s) echo ${system} ;;\n  -m) echo ${machine} ;;\n  *) exit 1 ;;\nesac\n`,
  );
  chmodSync(uname, 0o755);
  return `${bin}${path.delimiter}${process.env.PATH}`;
}

function runInstaller(workspace, fixture, options = {}) {
  const env = {
    ...process.env,
    BOWLINE_RELEASE_HOST: `file://${fixture.root}`,
    BOWLINE_INSTALL_DIR: path.join(workspace, "bin"),
    HOME: workspace,
    PATH: options.path ?? process.env.PATH,
  };
  return run("sh", [fixture.installer, "--cli-only", ...(options.args ?? [])], {
    env,
  });
}

function expectSuccess(label, result) {
  if (result.status !== 0) {
    throw new Error(`${label} expected success\nstderr:\n${result.stderr}`);
  }
}

function expectFailure(label, result, expectedMessage) {
  if (result.status === 0) {
    throw new Error(`${label} expected failure`);
  }
  if (!result.stderr.includes(expectedMessage)) {
    throw new Error(
      `${label} expected ${expectedMessage}\nactual stderr:\n${result.stderr}`,
    );
  }
}

function runCase(label, mutate, expectedMessage, options = {}) {
  const { fixture, workspace } = makeFixture(options.fixtureTarget);
  try {
    mutate(fixture, workspace);
    const runOptions = options.platform
      ? {
          ...options,
          path: pathWithUname(
            workspace,
            options.platform.system,
            options.platform.machine,
          ),
        }
      : options;
    const result = runInstaller(workspace, fixture, runOptions);
    if (expectedMessage) {
      expectFailure(label, result, expectedMessage);
    } else {
      expectSuccess(label, result);
    }
    console.error(`[release-authenticity-smoke] ${label}: pass`);
  } finally {
    rmSync(workspace, { recursive: true, force: true });
  }
}

async function runReleaseAssetsProducerSmoke() {
  const workspace = mkdtempSync(
    path.join(tmpdir(), "bowline-release-producer-"),
  );
  const keyFile = path.join(workspace, "release-key");
  const distRoot = path.join(workspace, "dist");
  const releaseAssetsModuleUrl = `${pathToFileURL(path.join(sourceRoot, "scripts", "release-assets.mjs")).href}?smoke=${Date.now()}`;
  const previousDistRoot = process.env.BOWLINE_RELEASE_DIST_ROOT;
  process.env.BOWLINE_RELEASE_DIST_ROOT = distRoot;
  try {
    mustRun("ssh-keygen", [
      "-q",
      "-t",
      "ed25519",
      "-N",
      "",
      "-C",
      releaseSigningIdentity,
      "-f",
      keyFile,
    ]);
    const releaseAssets = await import(releaseAssetsModuleUrl);
    const releaseDir = releaseAssets.releaseDist(version);
    mkdirSync(releaseDir, { recursive: true });
    const archive = path.join(releaseDir, `bowline-${target}.tar.gz`);
    writeArchive(workspace, archive);
    writeFileSync(path.join(releaseDir, "checksums.txt.sig"), "stale");
    writeFileSync(path.join(releaseDir, "release-manifest.json.sig"), "stale");

    await releaseAssets.cleanGeneratedReleaseRoots(version);
    if (existsSync(path.join(releaseDir, "checksums.txt.sig"))) {
      throw new Error("stale checksums signature was not removed");
    }
    if (existsSync(path.join(releaseDir, "release-manifest.json.sig"))) {
      throw new Error("stale manifest signature was not removed");
    }

    await releaseAssets.stageInstaller(version);
    const externalArchive = path.join(
      workspace,
      "external",
      `bowline-${target}.tar.gz`,
    );
    mkdirSync(path.dirname(externalArchive), { recursive: true });
    writeFileSync(externalArchive, "first external archive");
    await releaseAssets.stageExternalArtifacts(version, [externalArchive]);
    if (readFileSync(archive, "utf8") !== "first external archive") {
      throw new Error("external artifact first staging wrote unexpected bytes");
    }
    writeFileSync(externalArchive, "second external archive");
    await releaseAssets.stageExternalArtifacts(version, [externalArchive]);
    if (readFileSync(archive, "utf8") !== "second external archive") {
      throw new Error("external artifact rerun did not refresh staged bytes");
    }

    const {
      checksums,
      manifest,
      releaseAssets: signedAssets,
    } = await releaseAssets.writeSignedReleaseRoots(version, "normal", keyFile);
    const allowed = path.join(workspace, "allowed-signers");
    writeFileSync(
      allowed,
      releaseAllowedSignersLine(readFileSync(`${keyFile}.pub`, "utf8")),
    );
    verifyReleaseFile(checksums, `${checksums}.sig`, allowed);
    verifyReleaseFile(manifest, `${manifest}.sig`, allowed);

    const checksumRows = readFileSync(checksums, "utf8");
    if (checksumRows.includes(".sig")) {
      throw new Error("checksums.txt must not contain signature rows");
    }
    const manifestBody = JSON.parse(readFileSync(manifest, "utf8"));
    if (!manifestBody.artifacts.checksums_sig) {
      throw new Error("release manifest is missing checksums_sig");
    }
    if (manifestBody.artifacts.manifest_sig) {
      throw new Error("release manifest must not contain manifest_sig");
    }

    for (const channel of ["versioned", "latest"]) {
      const plan = releaseAssets.releaseUploadPlan(
        version,
        signedAssets,
        channel,
      );
      const keys = plan.map((item) => item.key);
      if (!keys.at(-1)?.endsWith("release-manifest.json")) {
        throw new Error(`${channel} upload plan must publish manifest last`);
      }
      const manifestIndex = keys.findIndex((key) =>
        key.endsWith("release-manifest.json"),
      );
      const sigIndex = keys.findIndex((key) =>
        key.endsWith("release-manifest.json.sig"),
      );
      if (sigIndex === -1 || sigIndex > manifestIndex) {
        throw new Error(
          `${channel} upload plan must publish manifest signature before manifest`,
        );
      }
    }
    console.error("[release-authenticity-smoke] release-assets producer: pass");
  } finally {
    if (previousDistRoot === undefined) {
      delete process.env.BOWLINE_RELEASE_DIST_ROOT;
    } else {
      process.env.BOWLINE_RELEASE_DIST_ROOT = previousDistRoot;
    }
    rmSync(workspace, { recursive: true, force: true });
  }
}

assertSshsigCli();
await runReleaseAssetsProducerSmoke();
runCase("clean signed fixture", () => undefined, null);
runCase("clean signed Linux x86_64 fixture", () => undefined, null, {
  fixtureTarget: "x86_64-unknown-linux-gnu",
  platform: { system: "Linux", machine: "x86_64" },
});
runCase("clean signed Linux ARM fixture", () => undefined, null, {
  fixtureTarget: "aarch64-unknown-linux-gnu",
  platform: { system: "Linux", machine: "aarch64" },
});
runCase(
  "missing root manifest signature",
  ({ root }) => {
    rmSync(path.join(root, "release-manifest.json.sig"), { force: true });
  },
  "download release-manifest.json.sig",
);
runCase(
  "tampered latest manifest",
  ({ root }) => {
    const manifest = path.join(root, "release-manifest.json");
    const body = JSON.parse(readFileSync(manifest, "utf8"));
    body.version = "9.9.9";
    writeFileSync(manifest, `${JSON.stringify(body, null, 2)}\n`);
  },
  "signature verification failed for release-manifest.json",
);
runCase(
  "missing pinned manifest signature",
  ({ releaseDir }) => {
    rmSync(path.join(releaseDir, "release-manifest.json.sig"), { force: true });
  },
  "download release-manifest.json.sig",
  { args: ["--version", version] },
);
runCase(
  "missing v-prefixed pinned manifest signature",
  ({ releaseDir }) => {
    rmSync(path.join(releaseDir, "release-manifest.json.sig"), { force: true });
  },
  "download release-manifest.json.sig",
  { args: ["--version", `v${version}`] },
);
runCase(
  "signed pinned manifest wrong version",
  ({ keyFile, manifest }) => {
    const body = JSON.parse(readFileSync(manifest, "utf8"));
    body.version = "9.9.9";
    writeFileSync(manifest, `${JSON.stringify(body, null, 2)}\n`);
    signReleaseFile(manifest, keyFile, { capture: true });
  },
  `release manifest version 9.9.9 does not match requested ${version}`,
  { args: ["--version", version] },
);
runCase(
  "matching malicious checksum with stale signature",
  ({ archive, checksums }) => {
    writeFileSync(archive, "malicious archive");
    writeFileSync(
      checksums,
      `${sha256File(archive)}  ${path.basename(archive)}\n`,
    );
  },
  "release manifest hash mismatch for checksums.txt",
);
runCase(
  "corrupt checksum signature",
  ({ checksums, keyFile, manifest, root }) => {
    writeFileSync(`${checksums}.sig`, "not an ssh signature");
    const body = JSON.parse(readFileSync(manifest, "utf8"));
    body.artifacts.checksums_sig.sha256 = sha256File(`${checksums}.sig`);
    writeFileSync(manifest, `${JSON.stringify(body, null, 2)}\n`);
    signReleaseFile(manifest, keyFile, { capture: true });
    copyFileSync(manifest, path.join(root, "release-manifest.json"));
    copyFileSync(
      `${manifest}.sig`,
      path.join(root, "release-manifest.json.sig"),
    );
  },
  "signature verification failed for checksums.txt",
);
runCase(
  "archive byte flip",
  ({ archive }) => {
    writeFileSync(archive, "tampered archive");
  },
  "checksum mismatch",
);
runCase(
  "replayed checksum root under current manifest",
  ({ archive, checksums, keyFile }) => {
    writeFileSync(archive, "older signed archive");
    writeFileSync(
      checksums,
      `${sha256File(archive)}  ${path.basename(archive)}\n`,
    );
    signReleaseFile(checksums, keyFile, { capture: true });
  },
  "release manifest hash mismatch for checksums.txt",
);
runCase(
  "missing ssh-keygen verifier",
  () => undefined,
  "ssh-keygen is required",
  {
    path: constrainedPath(mkdtempSync(path.join(tmpdir(), "bowline-no-ssh-"))),
  },
);

console.error("[release-authenticity-smoke] all cases passed");
