import { execFileSync } from "node:child_process";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  rm,
} from "node:fs/promises";
import path from "node:path";

const ignoredDirectorySegments = new Set([
  ".turbo",
  ".wrangler",
  "coverage",
  "dist",
  "node_modules",
  "target",
]);

const ignoredFilePatterns = [
  /\.tsbuildinfo$/u,
  /(^|\/)\.DS_Store$/u,
  /(^|\/)(npm-debug|yarn-error|pnpm-debug)\.log$/u,
];

function parseArgs(argv) {
  const args = {
    manifest: "public-export.json",
    source: process.cwd(),
    target: null,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--manifest") {
      args.manifest = argv[++index] ?? null;
    } else if (arg === "--source") {
      args.source = argv[++index] ?? null;
    } else if (arg === "--target") {
      args.target = argv[++index] ?? null;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.manifest) throw new Error("--manifest requires a path");
  if (!args.source) throw new Error("--source requires a path");
  if (!args.target) throw new Error("--target requires a public repo path");
  return args;
}

function git(root, args) {
  return execFileSync("git", ["-C", root, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function isInside(child, parent) {
  const relative = path.relative(parent, child);
  return (
    relative === "" ||
    (!relative.startsWith("..") && !path.isAbsolute(relative))
  );
}

function assertRelativePath(entry) {
  if (typeof entry !== "string" || entry.length === 0) {
    throw new Error("public-export.json entries must be non-empty strings");
  }
  if (path.isAbsolute(entry) || entry.split(/[\\/]/u).includes("..")) {
    throw new Error(`Export path must be repo-relative: ${entry}`);
  }
}

async function readManifest(manifestPath) {
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  if (!Array.isArray(manifest.include)) {
    throw new Error("public-export.json must contain an include array");
  }

  for (const entry of manifest.include) assertRelativePath(entry);
  return manifest.include;
}

async function assertGitRepo(root, label) {
  try {
    git(root, ["rev-parse", "--git-dir"]);
  } catch {
    throw new Error(`${label} must be a git working tree: ${root}`);
  }
}

function assertCleanTarget(targetRoot) {
  const status = git(targetRoot, ["status", "--porcelain"]);
  if (status.length > 0) {
    throw new Error(
      "Target repo has uncommitted changes; review or reset them before export",
    );
  }
}

async function pruneTarget(targetRoot) {
  const entries = await readdir(targetRoot);
  await Promise.all(
    entries
      .filter((entry) => entry !== ".git")
      .map((entry) =>
        rm(path.join(targetRoot, entry), { recursive: true, force: true }),
      ),
  );
}

async function copyEntry(sourcePath, targetPath) {
  const normalizedSource = sourcePath.split(path.sep).join("/");
  if (ignoredFilePatterns.some((pattern) => pattern.test(normalizedSource))) {
    return;
  }

  const stat = await lstat(sourcePath);

  if (stat.isSymbolicLink()) {
    throw new Error(`Symlinks are not exported: ${sourcePath}`);
  }

  if (stat.isDirectory()) {
    if (ignoredDirectorySegments.has(path.basename(sourcePath))) return;
    await mkdir(targetPath, { recursive: true });
    const entries = await readdir(sourcePath);
    for (const entry of entries) {
      if (entry === ".git") continue;
      await copyEntry(
        path.join(sourcePath, entry),
        path.join(targetPath, entry),
      );
    }
    return;
  }

  await mkdir(path.dirname(targetPath), { recursive: true });

  if (!stat.isFile()) return;

  await copyFile(sourcePath, targetPath);
  await chmod(targetPath, stat.mode);
}

async function validateEntry(sourcePath) {
  const normalizedSource = sourcePath.split(path.sep).join("/");
  if (ignoredFilePatterns.some((pattern) => pattern.test(normalizedSource))) {
    return;
  }

  const stat = await lstat(sourcePath);
  if (stat.isSymbolicLink()) {
    throw new Error(`Symlinks are not exported: ${sourcePath}`);
  }
  if (!stat.isDirectory()) return;
  if (ignoredDirectorySegments.has(path.basename(sourcePath))) return;

  const entries = await readdir(sourcePath);
  for (const entry of entries) {
    if (entry === ".git") continue;
    await validateEntry(path.join(sourcePath, entry));
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const sourceRoot = await realpath(path.resolve(args.source));
  const targetRoot = await realpath(path.resolve(args.target));
  const manifestPath = path.resolve(sourceRoot, args.manifest);

  await assertGitRepo(targetRoot, "Target");
  if (isInside(targetRoot, sourceRoot)) {
    throw new Error("Target repo must be outside the private source repo");
  }
  if (isInside(sourceRoot, targetRoot)) {
    throw new Error("Target repo must not contain the private source repo");
  }
  assertCleanTarget(targetRoot);

  const include = await readManifest(manifestPath);
  for (const entry of include) {
    try {
      await validateEntry(path.join(sourceRoot, entry));
    } catch (error) {
      if (error && error.code !== "ENOENT") throw error;
      throw new Error(`Allowlisted path does not exist: ${entry}`);
    }
  }

  await pruneTarget(targetRoot);
  for (const entry of include) {
    await copyEntry(path.join(sourceRoot, entry), path.join(targetRoot, entry));
  }

  console.log(
    `Exported ${include.length} allowlisted entries to ${targetRoot}`,
  );
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
