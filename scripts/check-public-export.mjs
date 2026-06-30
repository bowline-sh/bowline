import { execFileSync } from "node:child_process";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";

const defaultManifest = "public-export.json";
const maxTextBytes = 1024 * 1024;
const textExtensions = new Set([
  "",
  ".css",
  ".js",
  ".json",
  ".md",
  ".mjs",
  ".rs",
  ".toml",
  ".ts",
  ".tsx",
  ".txt",
  ".yaml",
  ".yml",
]);

const exactPathReasons = new Map([
  ["AGENTS.md", "prerelease agent instructions must stay private"],
  [
    ".github/workflows/deploy-public.yml",
    "public deploy workflow is private-only",
  ],
]);

const prefixReasons = new Map([
  ["docs/plans/", "private planning documents must stay private"],
  ["infra/cloudflare/", "Cloudflare deployment wiring must stay private"],
  [
    "infra/object-storage/",
    "object-storage deployment wiring must stay private",
  ],
]);

const segmentReasons = new Map([
  [".turbo", "local build cache output"],
  [".wrangler", "Cloudflare local output"],
  ["coverage", "test coverage output"],
  ["dist", "build output"],
  ["node_modules", "dependency install output"],
  ["reports", "generated/private reports must stay private"],
  ["research", "raw research must stay private"],
  ["target", "Rust build output"],
  ["transcripts", "conversation transcripts must stay private"],
]);

const filePatterns = [
  {
    pattern: /(^|\/)\.env($|[./-])/u,
    reason: "raw env files must stay private",
  },
  { pattern: /(^|\/)\.DS_Store$/u, reason: "local machine artifact" },
  { pattern: /\.tsbuildinfo$/u, reason: "TypeScript incremental build output" },
  {
    pattern: /(^|\/)(npm-debug|yarn-error|pnpm-debug)\.log$/u,
    reason: "package manager debug log",
  },
];

const ignoredExpansionSegments = new Set([
  ".turbo",
  ".wrangler",
  "coverage",
  "dist",
  "node_modules",
  "target",
]);
const ignoredExpansionFilePatterns = [
  /\.tsbuildinfo$/u,
  /(^|\/)\.DS_Store$/u,
  /(^|\/)(npm-debug|yarn-error|pnpm-debug)\.log$/u,
];

const privateHome = process.env.HOME?.split(path.sep).join("/") ?? null;
const contentPatterns = [
  ...(privateHome
    ? [
        {
          pattern: new RegExp(escapeRegExp(privateHome), "u"),
          reason: "private local absolute path",
        },
      ]
    : []),
  { pattern: /\/tmp\/compound-engineering/u, reason: "private scratch path" },
  {
    pattern: /\/(?:Users|home)\/[^/\s"'`]+/u,
    reason: "private local absolute path",
  },
];
const secretAssignmentPattern =
  /(^|\n)\s*[A-Z0-9_]*(SECRET|TOKEN|PASSWORD|PRIVATE_KEY|API_KEY|ACCESS_KEY)[A-Z0-9_]*\s*=\s*["']?[A-Za-z0-9_./+=:-]{12,}/u;

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
}

function parseArgs(argv) {
  const args = {
    allTracked: false,
    filesFrom: null,
    manifest: defaultManifest,
    root: process.cwd(),
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--all-tracked") {
      args.allTracked = true;
    } else if (arg === "--files-from") {
      args.filesFrom = argv[++index] ?? null;
    } else if (arg === "--manifest") {
      args.manifest = argv[++index] ?? null;
    } else if (arg === "--root") {
      args.root = argv[++index] ?? null;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (argv.includes("--files-from") && !args.filesFrom) {
    throw new Error("--files-from requires a fixture path");
  }
  if (!args.manifest) throw new Error("--manifest requires a path");
  if (!args.root) throw new Error("--root requires a path");
  return args;
}

function normalizeFilePath(filePath) {
  return filePath.split(path.sep).join("/");
}

function gitTrackedFiles(root) {
  const raw = execFileSync("git", ["-C", root, "ls-files", "-z"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return raw.split("\0").filter(Boolean);
}

async function listedFiles(filesFrom) {
  const fixture = await readFile(filesFrom, "utf8");
  return fixture
    .split(/\r?\n/u)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"));
}

async function pathExists(filePath) {
  try {
    await stat(filePath);
    return true;
  } catch {
    return false;
  }
}

async function expandEntry(root, entry) {
  const fullPath = path.join(root, entry);
  const entryStat = await stat(fullPath);
  if (entryStat.isFile()) {
    const normalized = normalizeFilePath(entry);
    return ignoredExpansionFilePatterns.some((pattern) =>
      pattern.test(normalized),
    )
      ? []
      : [entry];
  }
  if (!entryStat.isDirectory()) return [];
  if (ignoredExpansionSegments.has(path.basename(entry))) return [];

  const found = [];
  const children = await readdir(fullPath, { withFileTypes: true });
  for (const child of children) {
    if (child.name === ".git") continue;
    const childEntry = normalizeFilePath(path.join(entry, child.name));
    found.push(...(await expandEntry(root, childEntry)));
  }
  return found;
}

async function manifestFiles(root, manifestPath) {
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  if (!Array.isArray(manifest.include)) {
    throw new Error(`${manifestPath} must contain an include array`);
  }

  const expanded = [];
  for (const entry of manifest.include) {
    expanded.push(...(await expandEntry(root, entry)));
  }
  return expanded;
}

async function filesToCheck(root, filesFrom, manifest, allTracked) {
  if (filesFrom) return listedFiles(filesFrom);
  if (allTracked) return gitTrackedFiles(root);

  const manifestPath = path.resolve(root, manifest);
  if (await pathExists(manifestPath)) {
    return manifestFiles(root, manifestPath);
  }

  return gitTrackedFiles(root);
}

function pathReason(filePath) {
  const normalized = normalizeFilePath(filePath);
  const exact = exactPathReasons.get(normalized);
  if (exact) return exact;

  for (const [prefix, reason] of prefixReasons) {
    if (normalized.startsWith(prefix)) return reason;
  }

  const segments = normalized.split("/");
  for (const segment of segments) {
    const reason = segmentReasons.get(segment);
    if (reason) return reason;
  }

  for (const { pattern, reason } of filePatterns) {
    if (pattern.test(normalized)) return reason;
  }

  return null;
}

async function contentReason(root, filePath) {
  const fullPath = path.join(root, filePath);
  let fileStat;
  try {
    fileStat = await stat(fullPath);
  } catch {
    return null;
  }

  if (!fileStat.isFile() || fileStat.size > maxTextBytes) return null;
  if (!textExtensions.has(path.extname(filePath))) return null;

  const content = await readFile(fullPath, "utf8");
  if (content.includes("\0")) return null;

  for (const { pattern, reason } of contentPatterns) {
    if (pattern.test(content)) return reason;
  }
  if (secretAssignmentPattern.test(content)) return "secret-looking assignment";

  return null;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const root = path.resolve(args.root);
  const files = await filesToCheck(
    root,
    args.filesFrom,
    args.manifest,
    args.allTracked,
  );
  const errors = [];

  for (const file of files) {
    const normalized = normalizeFilePath(file);
    const reason =
      pathReason(normalized) ?? (await contentReason(root, normalized));
    if (reason) errors.push(`${normalized}: ${reason}`);
  }

  if (errors.length > 0) {
    console.error(errors.join("\n"));
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(2);
});
