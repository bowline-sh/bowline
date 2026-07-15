import { spawnSync } from "node:child_process";
import { readdir } from "node:fs/promises";
import path from "node:path";

const generatedPathSegments = new Map([
  ["node_modules", "dependency install output"],
  [".turbo", "turbo cache output"],
  ["dist", "build output"],
  ["target", "Rust build output"],
  ["coverage", "test coverage output"],
  [".wrangler", "Cloudflare local output"],
]);
const generatedFilePatterns = [
  { pattern: /\.tsbuildinfo$/u, reason: "TypeScript incremental build output" },
  { pattern: /(^|\/)\.DS_Store$/u, reason: "macOS Finder metadata" },
  {
    pattern: /(^|\/)(?:npm-debug|yarn-error|pnpm-debug)\.log$/u,
    reason: "package manager debug log",
  },
];
const sourceRoots = [
  "apps",
  "packages",
  "crates",
  "scripts",
  "infra",
  "tests",
  "docs",
  "plans",
  "examples",
];

function parseArgs(argv) {
  if (argv.length === 0) return { root: process.cwd() };
  if (argv.length === 2 && argv[0] === "--root")
    return { root: path.resolve(argv[1]) };
  throw new Error(
    "Usage: node scripts/check-generated-artifacts.mjs [--root <source-root>]",
  );
}

function normalize(filePath) {
  return filePath.split(path.sep).join("/");
}

function gitFiles(root) {
  const topLevel = spawnSync(
    "git",
    ["-C", root, "rev-parse", "--show-toplevel"],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    },
  );
  if (
    topLevel.status !== 0 ||
    path.resolve(topLevel.stdout.trim()) !== path.resolve(root)
  )
    return null;
  const result = spawnSync("git", ["-C", root, "ls-files", "-z"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  if (result.status !== 0) return null;
  return result.stdout.split("\0").filter(Boolean);
}

async function walk(root, relative = "") {
  const directory = path.join(root, relative);
  let entries;
  try {
    entries = await readdir(directory, { withFileTypes: true });
  } catch (error) {
    if (error?.code === "ENOENT") return [];
    throw error;
  }
  const files = [];
  for (const entry of entries) {
    if (entry.name === ".git") continue;
    const child = path.join(relative, entry.name);
    if (entry.isDirectory()) files.push(...(await walk(root, child)));
    else if (entry.isFile()) files.push(normalize(child));
  }
  return files;
}

async function sourceFiles(root) {
  const tracked = gitFiles(root);
  if (tracked) return tracked;
  const files = [];
  for (const sourceRoot of sourceRoots)
    files.push(...(await walk(root, sourceRoot)));
  return files;
}

function generatedReason(filePath) {
  const normalized = normalize(filePath);
  if (normalized.startsWith("tests/fixtures/generated-artifacts/")) return null;
  const inSourceRoot = sourceRoots.some(
    (root) => normalized === root || normalized.startsWith(`${root}/`),
  );
  if (inSourceRoot) {
    for (const segment of normalized.split("/")) {
      const reason = generatedPathSegments.get(segment);
      if (reason) return reason;
    }
  }
  for (const { pattern, reason } of generatedFilePatterns) {
    if (pattern.test(normalized)) return reason;
  }
  return null;
}

const { root } = parseArgs(process.argv.slice(2));
const errors = [];
for (const file of await sourceFiles(root)) {
  const reason = generatedReason(file);
  if (reason)
    errors.push(
      `${file}: tracked/source-archive generated artifact (${reason})`,
    );
}
if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exitCode = 1;
}
