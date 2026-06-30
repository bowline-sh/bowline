import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
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
  { pattern: /(^|\/)npm-debug\.log$/u, reason: "package manager debug log" },
  { pattern: /(^|\/)yarn-error\.log$/u, reason: "package manager debug log" },
  { pattern: /(^|\/)pnpm-debug\.log$/u, reason: "package manager debug log" },
];

const sourceRoots = [
  "apps/",
  "packages/",
  "crates/",
  "scripts/",
  "infra/",
  "tests/",
  "docs/",
];

function parseArgs(argv) {
  const filesFromIndex = argv.indexOf("--files-from");
  return {
    filesFrom:
      filesFromIndex === -1 ? null : (argv[filesFromIndex + 1] ?? null),
  };
}

function normalizeTrackedPath(filePath) {
  return filePath.split(path.sep).join("/");
}

async function trackedFiles(filesFrom) {
  if (filesFrom) {
    const fixture = await readFile(filesFrom, "utf8");
    return fixture
      .split(/\r?\n/u)
      .map((line) => line.trim())
      .filter((line) => line.length > 0 && !line.startsWith("#"));
  }

  const raw = execFileSync("git", ["ls-files", "-z"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });

  return raw.split("\0").filter(Boolean);
}

function generatedReason(filePath) {
  const normalized = normalizeTrackedPath(filePath);
  const inSourceRoot = sourceRoots.some((root) => normalized.startsWith(root));
  const segments = normalized.split("/");

  for (const segment of segments) {
    const reason = generatedPathSegments.get(segment);
    if (reason && inSourceRoot) return reason;
  }

  for (const { pattern, reason } of generatedFilePatterns) {
    if (pattern.test(normalized)) return reason;
  }

  return null;
}

const { filesFrom } = parseArgs(process.argv.slice(2));
if (filesFrom === null && process.argv.includes("--files-from")) {
  console.error("--files-from requires a fixture path");
  process.exit(2);
}

const errors = [];
for (const file of await trackedFiles(filesFrom)) {
  const reason = generatedReason(file);
  if (reason) errors.push(`${file}: tracked generated artifact (${reason})`);
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}
