import { existsSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

// Single owner of file length for every language in the repo. eslint's
// max-lines deliberately does not exist: two gates for one rule drift, and the
// one that walked only crates/ exempted the largest files in the codebase.
const ROOT = path.resolve(import.meta.dirname, "..");

const SOURCE_ROOTS = ["apps", "crates", "packages", "scripts", "tests"];

const SKIPPED_DIRECTORIES = new Set([
  ".build",
  ".source",
  ".turbo",
  ".wrangler",
  "_generated",
  "coverage",
  "dist",
  "fixtures",
  "generated",
  "node_modules",
  "target",
]);

const LIMITS = [
  { extensions: [".rs"], source: 900, test: 2200 },
  {
    extensions: [".mjs", ".js", ".cjs", ".ts", ".tsx"],
    source: 700,
    test: 1000,
  },
];

// Ratchets for files that predate this gate. They may only shrink or be
// deleted; never add an entry and never raise one (see AGENTS.md).
const RATCHETS = new Map([
  ["apps/web/src/components/marketing/sections/competitors-data.ts", 750],
  ["apps/web/src/dashboard/data/dashboard-data.ts", 800],
  ["apps/web/src/routeTree.gen.ts", 1200],
  ["packages/control-plane/convex-tests/billing.convex.test.ts", 1050],
  [
    "packages/control-plane/convex-tests/workspace-memberships.convex.test.ts",
    1310,
  ],
  ["packages/control-plane/convex/agent_auth.ts", 750],
  ["packages/control-plane/convex/billing.ts", 1100],
  ["packages/control-plane/convex/dashboard.ts", 850],
  ["packages/control-plane/convex/devices.ts", 1120],
  ["packages/control-plane/convex/lib/dashboardProjections.ts", 800],
  ["scripts/hosted-daemon-loop-smoke.mjs", 800],
  ["scripts/release/candidate-install.mjs", 900],
  ["scripts/release/stages/build-ship.mjs", 800],
  ["scripts/release/stages/device-journeys.mjs", 1050],
]);

const errors = [];

for (const root of SOURCE_ROOTS) {
  // The public export carries a subset of these roots — it ships no `apps/` —
  // and this gate runs there too. A root that is absent contributes no files
  // to measure, which is a different thing from a root that is empty or
  // unreadable; both of those still walk.
  if (!existsSync(path.join(ROOT, root))) continue;
  for await (const file of walk(path.join(ROOT, root))) {
    const relative = slash(path.relative(ROOT, file));
    const max = maxLinesFor(relative);
    if (max === null) continue;

    const lines = await countLines(file);
    if (lines > max) {
      errors.push(`${relative}: ${lines} lines exceeds ${max}`);
    }
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}

function isTest(relative) {
  return (
    relative.includes("/tests/") ||
    relative.endsWith("/tests.rs") ||
    /(^|\/)[^/]*\.test\.[^/]+$/u.test(relative) ||
    relative.includes("-tests/")
  );
}

function maxLinesFor(relative) {
  const ratchet = RATCHETS.get(relative);
  if (ratchet !== undefined) return ratchet;

  const extension = path.extname(relative);
  const limit = LIMITS.find((entry) => entry.extensions.includes(extension));
  if (!limit) return null;
  // Rust caps only apply inside a crate's src/ or tests/ tree.
  if (extension === ".rs" && !relative.includes("/src/") && !isTest(relative)) {
    return null;
  }
  return isTest(relative) ? limit.test : limit.source;
}

async function countLines(file) {
  const source = await readFile(file, "utf8");
  if (source.length === 0) return 0;
  return source.endsWith("\n")
    ? source.split("\n").length - 1
    : source.split("\n").length;
}

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    if (SKIPPED_DIRECTORIES.has(entry.name)) continue;

    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(fullPath);
    } else {
      yield fullPath;
    }
  }
}

function slash(value) {
  return value.split(path.sep).join("/");
}
