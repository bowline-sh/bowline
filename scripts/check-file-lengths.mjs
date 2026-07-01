import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "..");
const DEFAULT_RUST_SOURCE_MAX = 900;
const DEFAULT_RUST_TEST_MAX = 2200;

const errors = [];

for await (const file of walk(path.join(ROOT, "crates"))) {
  if (!file.endsWith(".rs")) continue;

  const relative = slash(path.relative(ROOT, file));
  const max = maxLinesFor(relative);
  if (max === null) continue;

  const lines = await countLines(file);
  if (lines > max) {
    errors.push(`${relative}: ${lines} lines exceeds ${max}`);
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}

function maxLinesFor(relative) {
  if (relative.includes("/tests/") || relative.endsWith("/tests.rs")) {
    return DEFAULT_RUST_TEST_MAX;
  }
  if (relative.includes("/src/")) return DEFAULT_RUST_SOURCE_MAX;
  return null;
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
    if (entry.name === "target") continue;

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
