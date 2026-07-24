#!/usr/bin/env node
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { chmod, copyFile, mkdir, readdir, rm } from "node:fs/promises";
import path from "node:path";
import { isEntrypoint } from "./entrypoint.mjs";
import { signReleaseFile } from "./release-signing.mjs";
import { assertReleaseVersion } from "./release-version.mjs";
import { registry } from "./verify.mjs";
import { reuseDecision } from "./verify-receipt.mjs";

const sourceRoot = process.cwd();
const installHost = "https://install.bowline.sh";
const releaseDistRoot = process.env.BOWLINE_RELEASE_DIST_ROOT
  ? path.resolve(process.env.BOWLINE_RELEASE_DIST_ROOT)
  : path.join(sourceRoot, "dist", "public-release");
const releaseAssetPattern =
  /^(install\.sh|checksums\.txt|checksums\.txt\.sig|release-manifest\.json|release-manifest\.json\.sig|appcast\.xml|BowlineMenuBar\.pkg|Bowline-.+\.app\.zip|bowline-.+\.tar\.(?:gz|xz))$/u;
const generatedReleaseRootAssets = new Set([
  "checksums.txt",
  "checksums.txt.sig",
  "install-headless.sh",
  "install.sh",
  "release-manifest.json",
  "release-manifest.json.sig",
]);

function parseArgs(argv) {
  const args = {
    allowDirty: false,
    artifacts: [],
    bucket:
      process.env.BOWLINE_RELEASE_ASSETS_BUCKET ?? "bowline-release-assets",
    publish: false,
    publicRepo: path.resolve(sourceRoot, "../public"),
    receipt: null,
    urgency: process.env.BOWLINE_RELEASE_URGENCY ?? "normal",
    version: null,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--") {
      continue;
    } else if (arg === "--allow-dirty") {
      args.allowDirty = true;
    } else if (arg === "--artifact") {
      args.artifacts.push(path.resolve(requiredValue(argv, ++index, arg)));
    } else if (arg === "--bucket") {
      args.bucket = requiredValue(argv, ++index, arg);
    } else if (arg === "--receipt") {
      args.receipt = path.resolve(requiredValue(argv, ++index, arg));
    } else if (arg === "--publish") {
      args.publish = true;
    } else if (arg === "--public-repo") {
      args.publicRepo = path.resolve(requiredValue(argv, ++index, arg));
    } else if (arg === "--urgency") {
      args.urgency = requiredValue(argv, ++index, arg);
    } else if (arg === "--version") {
      args.version = requiredValue(argv, ++index, arg);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.version) throw new Error("--version is required");
  if (!/^v?\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$/.test(args.version)) {
    throw new Error("--version must look like 0.1.0 or v0.1.0");
  }
  if (!["normal", "required"].includes(args.urgency)) {
    throw new Error("--urgency must be normal or required");
  }
  args.version = args.version.replace(/^v/, "");
  return args;
}

function requiredValue(argv, index, flag) {
  const value = argv[index];
  if (!value || value.startsWith("--"))
    throw new Error(`${flag} requires a value`);
  return value;
}

function run(command, args, options = {}) {
  step(`run ${command} ${args.join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? sourceRoot,
    encoding: "utf8",
    stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.status !== 0) {
    const stderr = options.capture ? `\n${result.stderr}` : "";
    throw new Error(`${command} ${args.join(" ")} failed${stderr}`);
  }
  return result.stdout?.trim() ?? "";
}

function git(root, args) {
  return execFileSync("git", ["-C", root, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function repoStatus(root) {
  return git(root, ["status", "--porcelain"]);
}

function assertRepo(root, label) {
  if (!existsSync(root)) throw new Error(`${label} repo is missing: ${root}`);
  git(root, ["rev-parse", "--git-dir"]);
}

function assertClean(root, label, allowDirty = false) {
  const status = repoStatus(root);
  if (status && !allowDirty) {
    throw new Error(
      `${label} repo has uncommitted changes; use --allow-dirty only for local dry runs`,
    );
  }
}

function assertPrivateRepoBoundary() {
  const agentsPath = path.join(sourceRoot, "AGENTS.md");
  const agents = readFileSync(agentsPath, "utf8");
  if (!agents.includes("This is Bowline's private canonical repo")) {
    throw new Error(
      "AGENTS.md must declare the private repo boundary before release",
    );
  }
}

function assertRemote(root, expectedSuffix, label) {
  const remotes = git(root, ["remote", "-v"]);
  if (!remotes.includes(expectedSuffix)) {
    throw new Error(`${label} repo remote must include ${expectedSuffix}`);
  }
}

function commitIfChanged(root, message) {
  if (!repoStatus(root)) return false;
  step(`commit ${message}`);
  git(root, ["add", "-A"]);
  git(root, ["commit", "-m", message]);
  return true;
}

function step(message) {
  console.error(`[release-assets] ${message}`);
}

function signingKeyFile(publish) {
  const keyFile = process.env.BOWLINE_RELEASE_SIGNING_KEY_FILE;
  if (keyFile) {
    const resolved = path.resolve(keyFile);
    if (!existsSync(resolved)) {
      throw new Error(`release signing key file does not exist: ${resolved}`);
    }
    return resolved;
  }
  if (process.env.BOWLINE_RELEASE_SIGNING_KEY && publish) {
    throw new Error(
      "BOWLINE_RELEASE_SIGNING_KEY_FILE is required for --publish; raw key material in BOWLINE_RELEASE_SIGNING_KEY is not accepted",
    );
  }
  if (publish) {
    throw new Error(
      "BOWLINE_RELEASE_SIGNING_KEY_FILE is required for --publish",
    );
  }
  step(
    "release signing key file not configured; dry run will not emit .sig files",
  );
  return null;
}

function signFile(file, keyFile) {
  return signReleaseFile(file, keyFile, { cwd: sourceRoot, log: step });
}

async function buildArchive(publicRepo, version) {
  const dist = releaseDist(version);
  const stage = path.join(dist, "stage");
  step(`build release archive in ${dist}`);
  await rm(stage, { recursive: true, force: true });
  await mkdir(stage, { recursive: true });

  run(
    "cargo",
    ["build", "--release", "-p", "bowline", "-p", "bowline-daemon"],
    {
      cwd: publicRepo,
    },
  );
  for (const name of ["bowline", "bowline-daemon"]) {
    await copyFile(
      path.join(publicRepo, "target", "release", name),
      path.join(stage, name),
    );
  }
  for (const name of ["LICENSE", "README.md"]) {
    await copyFile(path.join(publicRepo, name), path.join(stage, name));
  }

  const target = `${process.arch === "arm64" ? "aarch64" : "x86_64"}-${process.platform === "darwin" ? "apple-darwin" : "unknown-linux-gnu"}`;
  const archive = path.join(dist, `bowline-${target}.tar.gz`);
  await rm(archive, { force: true });
  run("tar", [
    "-C",
    stage,
    "-czf",
    archive,
    "bowline",
    "bowline-daemon",
    "LICENSE",
    "README.md",
  ]);
  return { archive, sha256: sha256File(archive), target };
}

async function stageInstaller(version) {
  const dist = releaseDist(version);
  const installer = path.join(dist, "install.sh");
  await copyFile(path.join(sourceRoot, "scripts", "install.sh"), installer);
  await chmod(installer, 0o755);
  return { installer };
}

async function stageMacosArtifacts(version, publish) {
  const dist = releaseDist(version);
  const macosDist = path.join(sourceRoot, "dist", "macos");
  const required = ["Bowline-aarch64-apple-darwin.app.zip", "appcast.xml"];
  const optional = ["BowlineMenuBar.pkg"];
  const staged = [];
  for (const name of [...required, ...optional]) {
    const source = path.join(macosDist, name);
    const target = path.join(dist, name);
    await rm(target, { force: true });
    if (!existsSync(source)) {
      const message = `missing macOS release artifact: ${source}`;
      if (publish && required.includes(name)) throw new Error(message);
      step(`${message}; skipped`);
      continue;
    }
    await copyFile(source, target);
    staged.push(target);
  }
  return staged;
}

async function stageExternalArtifacts(version, artifacts) {
  const dist = releaseDist(version);
  const staged = [];
  for (const artifact of artifacts) {
    const name = path.basename(artifact);
    if (generatedReleaseRootAssets.has(name)) {
      throw new Error(
        `Release root asset is generated by release-assets and cannot be passed with --artifact: ${name}`,
      );
    }
    if (!releaseAssetPattern.test(name)) {
      throw new Error(`Unsupported release artifact name: ${name}`);
    }
    if (!existsSync(artifact)) {
      throw new Error(`Release artifact does not exist: ${artifact}`);
    }
    const target = path.join(dist, name);
    await rm(target, { force: true });
    await copyFile(artifact, target);
    staged.push(target);
  }
  return staged;
}

function releaseDist(version) {
  return path.join(releaseDistRoot, `v${version}`);
}

async function discoverArchives(version) {
  const dist = releaseDist(version);
  const entries = await readdir(dist, { withFileTypes: true });
  return entries
    .filter(
      (entry) =>
        entry.isFile() && /^bowline-.+\.tar\.(?:gz|xz)$/.test(entry.name),
    )
    .map((entry) => {
      const archive = path.join(dist, entry.name);
      const target = entry.name
        .replace(/^bowline-/, "")
        .replace(/\.tar\.(?:gz|xz)$/, "");
      return { archive, sha256: sha256File(archive), target };
    })
    .sort((left, right) => left.target.localeCompare(right.target));
}

function assertPublishArchives(artifacts) {
  const targets = new Set(artifacts.map((artifact) => artifact.target));
  const requiredTargets = [
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
  ];
  const missing = requiredTargets.filter((target) => !targets.has(target));
  if (missing.length > 0) {
    throw new Error(
      `missing required release artifact(s): ${missing.map((target) => `bowline-${target}.tar.gz`).join(", ")}`,
    );
  }
}

async function discoverReleaseAssets(version) {
  const dist = releaseDist(version);
  const entries = await readdir(dist, { withFileTypes: true });
  return entries
    .filter((entry) => entry.isFile() && releaseAssetPattern.test(entry.name))
    .map((entry) => {
      const file = path.join(dist, entry.name);
      return { file, name: entry.name, sha256: sha256File(file) };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
}

async function writeReleaseManifest(version, assets, urgency) {
  const dist = releaseDist(version);
  const manifest = {
    version,
    publishedAt: new Date().toISOString(),
    notesUrl: `${installHost}/releases/v${version}/release-manifest.json`,
    urgency,
    artifacts: Object.fromEntries(
      assets.map((asset) => [
        manifestKey(asset.name),
        {
          url: releaseUrl(version, asset.name),
          sha256: asset.sha256,
        },
      ]),
    ),
  };
  const file = path.join(dist, "release-manifest.json");
  writeFileSync(file, `${JSON.stringify(manifest, null, 2)}\n`);
  return file;
}

async function writeChecksums(version, assets) {
  const dist = releaseDist(version);
  const lines = assets
    .filter(
      (asset) => asset.name !== "checksums.txt" && !asset.name.endsWith(".sig"),
    )
    .map((asset) => `${asset.sha256}  ${asset.name}`)
    .join("\n");
  const file = path.join(dist, "checksums.txt");
  writeFileSync(file, `${lines}\n`);
  return file;
}

function sha256File(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function releaseUrl(version, name) {
  return `${installHost}/releases/v${version}/${name}`;
}

function manifestKey(name) {
  if (name === "install.sh") return "installer";
  if (name === "checksums.txt") return "checksums";
  if (name === "checksums.txt.sig") return "checksums_sig";
  if (name === "release-manifest.json") return "manifest";
  if (name === "appcast.xml") return "macos_appcast";
  if (name === "BowlineMenuBar.pkg") return "macos_pkg";
  if (name === "Bowline-aarch64-apple-darwin.app.zip") {
    return "macos_app_aarch64";
  }
  if (
    name === "bowline-aarch64-apple-darwin.tar.gz" ||
    name === "bowline-aarch64-apple-darwin.tar.xz"
  ) {
    return "macos_cli_aarch64";
  }
  if (
    name === "bowline-x86_64-unknown-linux-gnu.tar.gz" ||
    name === "bowline-x86_64-unknown-linux-gnu.tar.xz"
  ) {
    return "linux_cli_x86_64";
  }
  if (
    name === "bowline-aarch64-unknown-linux-gnu.tar.gz" ||
    name === "bowline-aarch64-unknown-linux-gnu.tar.xz"
  ) {
    return "linux_cli_aarch64";
  }
  return name.replace(/[^0-9A-Za-z]+/gu, "_").replace(/^_|_$/gu, "");
}

function contentType(name) {
  if (name.endsWith(".json")) return "application/json";
  if (name.endsWith(".xml")) return "application/xml";
  if (name.endsWith(".sh")) return "application/x-sh";
  if (name.endsWith(".txt") || name.endsWith(".sig")) return "text/plain";
  if (name.endsWith(".zip")) return "application/zip";
  if (name.endsWith(".xz")) return "application/x-xz";
  if (name.endsWith(".pkg")) return "application/octet-stream";
  return "application/octet-stream";
}

function releaseUploadArgs(bucket, asset, key) {
  return [
    "exec",
    "wrangler",
    "r2",
    "object",
    "put",
    `${bucket}/${key}`,
    "--remote",
    "--file",
    asset.file,
    "--content-type",
    contentType(asset.name),
  ];
}

function uploadAsset(bucket, asset, key, publish) {
  const args = releaseUploadArgs(bucket, asset, key);
  if (!publish) {
    step(`would upload ${asset.file} to r2://${bucket}/${key}`);
    return;
  }
  run("pnpm", args);
}

async function cleanGeneratedReleaseRoots(version) {
  for (const name of generatedReleaseRootAssets) {
    await rm(path.join(releaseDist(version), name), { force: true });
  }
}

async function writeSignedReleaseRoots(version, urgency, keyFile) {
  const assetsBeforeChecksums = await discoverReleaseAssets(version);
  const checksums = await writeChecksums(version, assetsBeforeChecksums);
  signFile(checksums, keyFile);
  const assetsBeforeManifest = await discoverReleaseAssets(version);
  const manifest = await writeReleaseManifest(
    version,
    assetsBeforeManifest,
    urgency,
  );
  signFile(manifest, keyFile);
  return {
    checksums,
    manifest,
    releaseAssets: await discoverReleaseAssets(version),
  };
}

function orderedRootAssets(releaseAssets) {
  return [
    ...releaseAssets.filter(
      (asset) =>
        asset.name !== "release-manifest.json" &&
        asset.name !== "release-manifest.json.sig",
    ),
    ...releaseAssets.filter(
      (asset) => asset.name === "release-manifest.json.sig",
    ),
    ...releaseAssets.filter((asset) => asset.name === "release-manifest.json"),
  ];
}

function releaseUploadPlan(version, releaseAssets, channel) {
  const prefix =
    channel === "latest" ? "releases/latest" : `releases/v${version}`;
  return orderedRootAssets(releaseAssets).map((asset) => ({
    asset,
    key: `${prefix}/${asset.name}`,
  }));
}

function uploadReleasePlan(bucket, version, releaseAssets, channel, publish) {
  for (const item of releaseUploadPlan(version, releaseAssets, channel)) {
    uploadAsset(bucket, item.asset, item.key, publish);
  }
}

// The public verify pass re-runs the full release profile the private receipt
// already proved. Because the public export is deterministically derived from
// the private source, a fresh receipt covering `release` makes it redundant —
// but ONLY when the receipt actually bound the public tree being published. A
// receipt whose publicExportTreeSha is null (BOWLINE_PUBLIC_REPO was unset) or
// diverges from this repo's HEAD tree proves nothing about ../public, so we fall
// back to a real verify rather than letting a divergent checkout sail through.
async function verifyPublicCheckout(args) {
  if (args.receipt) {
    const decision = await reuseDecision({
      receiptPath: args.receipt,
      neededProfile: "release",
      registry,
      root: sourceRoot,
    });
    const receipt = readReceiptFile(args.receipt);
    const currentPublicTreeSha = git(args.publicRepo, [
      "rev-parse",
      "HEAD^{tree}",
    ]);
    if (
      decision.reuse &&
      publicExportMatchesReceipt(receipt, currentPublicTreeSha)
    ) {
      step(`reusing verification receipt ${decision.shortHash}`);
      return;
    }
    const reason = decision.reuse
      ? "receipt did not bind this public checkout's tree"
      : decision.reason;
    step(
      `verification receipt not reusable (${reason}); verifying public checkout`,
    );
  }
  step("verify public checkout");
  run("pnpm", ["verify"], { cwd: args.publicRepo });
}

// Fail-closed reuse gate for the public verify: only a receipt that bound a
// non-null public export tree SHA equal to the public repo actually being
// published may stand in for `pnpm verify` there.
export function publicExportMatchesReceipt(receipt, currentPublicTreeSha) {
  const bound = receipt?.publicExportTreeSha ?? null;
  return Boolean(bound) && bound === currentPublicTreeSha;
}

function readReceiptFile(receiptPath) {
  try {
    return JSON.parse(readFileSync(receiptPath, "utf8"));
  } catch {
    return null;
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  assertReleaseVersion(args.version, sourceRoot);
  const keyFile = signingKeyFile(args.publish);
  step(`start v${args.version}${args.publish ? " publish" : " dry run"}`);
  assertPrivateRepoBoundary();
  assertRepo(sourceRoot, "Private source");
  assertRepo(args.publicRepo, "Public");
  assertRemote(
    sourceRoot,
    "github.com/crowlabs-dev/bowline-private.git",
    "Private source",
  );
  assertRemote(args.publicRepo, "github.com/bowline-sh/bowline.git", "Public");
  assertClean(sourceRoot, "Private source", args.allowDirty);
  assertClean(args.publicRepo, "Public");

  step(`export public source to ${args.publicRepo}`);
  run("pnpm", ["export:public", "--target", args.publicRepo]);
  step("install public dependencies");
  run("pnpm", ["install", "--frozen-lockfile"], { cwd: args.publicRepo });
  await verifyPublicCheckout(args);
  const sourceSha = git(sourceRoot, ["rev-parse", "HEAD"]);
  const publicChanged = commitIfChanged(
    args.publicRepo,
    `chore: sync public export for v${args.version} from ${sourceSha}`,
  );

  const artifact = await buildArchive(args.publicRepo, args.version);
  await cleanGeneratedReleaseRoots(args.version);
  await stageInstaller(args.version);
  await stageMacosArtifacts(args.version, args.publish);
  await stageExternalArtifacts(args.version, args.artifacts);
  const archives = await discoverArchives(args.version);
  if (args.publish) assertPublishArchives(archives);
  const { releaseAssets } = await writeSignedReleaseRoots(
    args.version,
    args.urgency,
    keyFile,
  );

  uploadReleasePlan(
    args.bucket,
    args.version,
    releaseAssets,
    "versioned",
    args.publish,
  );
  uploadReleasePlan(
    args.bucket,
    args.version,
    releaseAssets,
    "latest",
    args.publish,
  );

  console.log(
    JSON.stringify(
      {
        ok: true,
        publish: args.publish,
        publicChanged,
        artifact,
        archives,
        releaseAssets,
        bucket: args.bucket,
        next: args.publish
          ? "release assets uploaded to install.bowline.sh bucket"
          : "dry run complete; no upload, no visibility change",
      },
      null,
      2,
    ),
  );
}

if (isEntrypoint(import.meta.url)) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  });
}

export {
  cleanGeneratedReleaseRoots,
  discoverReleaseAssets,
  releaseDist,
  releaseUploadArgs,
  releaseUploadPlan,
  stageExternalArtifacts,
  stageInstaller,
  writeSignedReleaseRoots,
};
