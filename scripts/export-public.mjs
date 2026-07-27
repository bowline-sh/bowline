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
  writeFile,
} from "node:fs/promises";
import path from "node:path";

const publicRootScripts = ["build", "lint", "test", "typecheck"];

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
    maxBuffer: 64 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

// Only tracked files are exportable. Walking the filesystem would carry
// untracked scratch files — notes, keys, backups — out of the private repo the
// moment they landed inside an allowlisted directory.
function trackedFiles(root, entry) {
  const listing = execFileSync(
    "git",
    ["-C", root, "ls-files", "-z", "--cached", "--", entry],
    {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  return listing.split("\0").filter((line) => line.length > 0);
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

async function copyPublicOverrides(sourceRoot, targetRoot) {
  for (const file of trackedFiles(sourceRoot, "public-overrides")) {
    const output = path.relative("public-overrides", file);
    await copyTrackedFile(
      path.join(sourceRoot, file),
      path.join(targetRoot, output),
    );
  }
}

async function rewritePublicRootPackage(targetRoot) {
  const packagePath = path.join(targetRoot, "package.json");
  let pkg;
  try {
    pkg = JSON.parse(await readFile(packagePath, "utf8"));
  } catch (error) {
    if (error && error.code === "ENOENT") return;
    throw error;
  }
  const privateScripts = pkg.scripts ?? {};
  const scripts = {};
  for (const name of publicRootScripts) {
    if (typeof privateScripts[name] === "string") {
      scripts[name] = privateScripts[name];
    }
  }
  scripts.verify = [
    "node scripts/check-toolchain-declarations.mjs",
    "pnpm --filter @bowline/config test",
    "pnpm --filter @bowline/contracts test",
    "pnpm --filter @bowline/contracts build",
    "pnpm --filter @bowline/control-plane typecheck",
    "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/bowline-public-target} CARGO_INCREMENTAL=0 cargo fmt --check",
    "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/bowline-public-target} CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --all-features -- -D warnings",
    "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/bowline-public-target} CARGO_INCREMENTAL=0 cargo test --workspace",
    "node scripts/check-generated-artifacts.mjs",
    "node scripts/check-architecture-imports.mjs",
    "node scripts/check-rust-boundaries.mjs",
    "node scripts/check-file-lengths.mjs",
    "node scripts/check-public-export.mjs",
    "node scripts/check-whitespace.mjs",
    "pnpm lint",
    "pnpm typecheck",
  ].join(" && ");
  await assertVerifyScriptsExist(targetRoot, scripts.verify);
  pkg.scripts = scripts;
  await writeFile(packagePath, `${JSON.stringify(pkg, null, 2)}\n`);
}

// The public tree gets its OWN cargo target directory. Both repos build crates
// with the same names, but each bakes its own `CARGO_MANIFEST_DIR` into the test
// binaries, so a fixture read relative to the manifest resolves against
// whichever tree compiled last. Sharing `/tmp/bowline-dev-target` therefore does
// not just cost rebuilds — it makes a private-repo test silently read the public
// repo's fixtures and fail with a plausible wrong answer that looks exactly like
// a contract regression.
//
// The verify line above is a hand-written list, so a script deleted from the
// private repo leaves a command the exported repo cannot run. That is only
// discovered by publishing, which is the most expensive place to find it:
// `check-examples.mjs` was deleted in 84c286a2 and this list kept calling it
// until a release failed at the deploy stage. Fail here, where the export is
// still cheap to fix.
async function assertVerifyScriptsExist(targetRoot, verify) {
  const referenced = [...verify.matchAll(/node (scripts\/[\w.-]+\.mjs)/g)].map(
    (match) => match[1],
  );
  const missing = [];
  for (const script of referenced) {
    try {
      await lstat(path.join(targetRoot, script));
    } catch {
      missing.push(script);
    }
  }
  if (missing.length > 0) {
    throw new Error(
      `public verify references scripts the export does not contain: ${missing.join(", ")}`,
    );
  }
}

async function copyTrackedFile(sourcePath, targetPath) {
  const stat = await lstat(sourcePath);
  if (stat.isSymbolicLink()) {
    throw new Error(`Symlinks are not exported: ${sourcePath}`);
  }
  if (!stat.isFile()) return;

  await mkdir(path.dirname(targetPath), { recursive: true });
  await copyFile(sourcePath, targetPath);
  await chmod(targetPath, stat.mode);
}

async function assertExportable(sourcePath) {
  const stat = await lstat(sourcePath);
  if (stat.isSymbolicLink()) {
    throw new Error(`Symlinks are not exported: ${sourcePath}`);
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
  await assertGitRepo(sourceRoot, "Source");

  const include = await readManifest(manifestPath);
  const exports = new Map();
  for (const entry of include) {
    const files = trackedFiles(sourceRoot, entry);
    if (files.length === 0) {
      throw new Error(`Allowlisted path is not tracked by git: ${entry}`);
    }
    for (const file of files) {
      await assertExportable(path.join(sourceRoot, file));
      exports.set(file, file);
    }
  }

  await pruneTarget(targetRoot);
  for (const file of exports.keys()) {
    await copyTrackedFile(
      path.join(sourceRoot, file),
      path.join(targetRoot, file),
    );
  }
  await copyPublicOverrides(sourceRoot, targetRoot);
  await rewritePublicRootPackage(targetRoot);

  console.log(
    `Exported ${include.length} allowlisted entries to ${targetRoot}`,
  );
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
