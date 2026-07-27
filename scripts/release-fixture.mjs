#!/usr/bin/env node
// Builds a release object layout with the production generator so installer
// smokes never parse bytes the real release never produced.
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { isEntrypoint } from "./entrypoint.mjs";

// release-assets.mjs resolves its dist root at module load, so each fixture
// needs its own module instance bound to its own staging directory.
async function loadReleaseAssets(distRoot) {
  process.env.BOWLINE_RELEASE_DIST_ROOT = distRoot;
  const moduleUrl = pathToFileURL(
    path.join(process.cwd(), "scripts", "release-assets.mjs"),
  ).href;
  return import(`${moduleUrl}?dist=${encodeURIComponent(distRoot)}`);
}

export async function materializeReleaseFixture(request) {
  const { archives, distRoot, keyFile, root, version } = request;
  const urgency = request.urgency ?? "normal";
  const releaseAssets = await loadReleaseAssets(distRoot);
  const dist = releaseAssets.releaseDist(version);
  mkdirSync(dist, { recursive: true });
  for (const archive of archives) {
    copyFileSync(archive, path.join(dist, path.basename(archive)));
  }

  await releaseAssets.cleanGeneratedReleaseRoots(version);
  await releaseAssets.stageInstaller(version);
  const signed = await releaseAssets.writeSignedReleaseRoots(
    version,
    urgency,
    keyFile,
  );

  const keys = [];
  for (const channel of releaseAssets.releaseChannelNames) {
    for (const item of releaseAssets.releaseUploadPlan(
      version,
      signed.releaseAssets,
      channel,
    )) {
      const destination = path.join(root, item.key);
      mkdirSync(path.dirname(destination), { recursive: true });
      copyFileSync(item.asset.file, destination);
      keys.push(item.key);
    }
  }

  return { checksums: signed.checksums, dist, keys, manifest: signed.manifest };
}

function parseArgs(argv) {
  const args = {
    archives: [],
    distRoot: null,
    keyFile: null,
    root: null,
    version: null,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--archive") args.archives.push(path.resolve(value));
    else if (flag === "--dist-root") args.distRoot = path.resolve(value);
    else if (flag === "--key") args.keyFile = path.resolve(value);
    else if (flag === "--root") args.root = path.resolve(value);
    else if (flag === "--version") args.version = value;
    else throw new Error(`Unknown argument: ${flag}`);
    index += 1;
  }
  for (const [name, value] of Object.entries(args)) {
    if (value === null) throw new Error(`--${name} is required`);
  }
  if (args.archives.length === 0) throw new Error("--archive is required");
  return args;
}

if (isEntrypoint(import.meta.url)) {
  const args = parseArgs(process.argv.slice(2));
  materializeReleaseFixture(args)
    .then((result) => {
      console.log(JSON.stringify({ keys: result.keys }, null, 2));
    })
    .catch((error) => {
      console.error(error instanceof Error ? error.message : String(error));
      process.exit(1);
    });
}
