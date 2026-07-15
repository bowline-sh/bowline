import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";

const roots = [
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
const ignoredDirectories = new Set([
  ".build",
  ".git",
  ".source",
  ".turbo",
  ".wrangler",
  "coverage",
  "dist",
  "node_modules",
  "target",
]);
const textExtensions = new Set([
  "",
  ".css",
  ".html",
  ".js",
  ".json",
  ".jsonc",
  ".md",
  ".mjs",
  ".rs",
  ".sh",
  ".swift",
  ".toml",
  ".ts",
  ".tsx",
  ".txt",
  ".yaml",
  ".yml",
]);
const maxBytes = 2 * 1024 * 1024;
const conflictMarker = /^(?:<{7}|={7}|>{7})(?: |$)/u;

function parseArgs(argv) {
  if (argv.length === 0) return process.cwd();
  if (argv.length === 2 && argv[0] === "--root") return path.resolve(argv[1]);
  throw new Error(
    "Usage: node scripts/check-whitespace.mjs [--root <source-root>]",
  );
}

async function walk(root, relative) {
  const full = path.join(root, relative);
  let entries;
  try {
    entries = await readdir(full, { withFileTypes: true });
  } catch (error) {
    if (error?.code === "ENOENT") return [];
    throw error;
  }
  const files = [];
  for (const entry of entries) {
    if (entry.isDirectory() && ignoredDirectories.has(entry.name)) continue;
    const child = path.join(relative, entry.name);
    if (child === path.join("plans", "archive")) continue;
    if (entry.isDirectory()) files.push(...(await walk(root, child)));
    else if (entry.isFile()) files.push(child);
  }
  return files;
}

const root = parseArgs(process.argv.slice(2));
const errors = [];
for (const sourceRoot of roots) {
  for (const relative of await walk(root, sourceRoot)) {
    if (!textExtensions.has(path.extname(relative))) continue;
    const metadata = await stat(path.join(root, relative));
    if (metadata.size > maxBytes) continue;
    const content = await readFile(path.join(root, relative), "utf8");
    if (content.includes("\0")) continue;
    const lines = content.split(/\r?\n/u);
    for (let index = 0; index < lines.length; index += 1) {
      const line = lines[index];
      if (/[\t ]+$/u.test(line))
        errors.push(`${relative}:${index + 1}: trailing whitespace`);
      if (conflictMarker.test(line))
        errors.push(`${relative}:${index + 1}: merge conflict marker`);
    }
    if (content.length > 0 && !content.endsWith("\n"))
      errors.push(`${relative}: missing final newline`);
  }
}
if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exitCode = 1;
}
