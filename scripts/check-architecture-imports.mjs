import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";

function parseRoots(argv) {
  const rootIndex = argv.indexOf("--root");
  if (rootIndex === -1) return ["apps", "packages"];

  const root = argv[rootIndex + 1];
  if (!root) {
    console.error("--root requires a path");
    process.exit(2);
  }

  return [root];
}

const roots = parseRoots(process.argv.slice(2));
const importPattern =
  /\b(?:import|export)\s+(?:type\s+)?(?:[^'"]*?\s+from\s+)?["']([^"']+)["']/g;
const errors = [];

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    if (["dist", "node_modules"].includes(entry.name)) continue;
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(fullPath);
    } else if (/\.(?:ts|tsx|mts|cts)$/.test(entry.name)) {
      yield fullPath;
    }
  }
}

function moduleRootForInternal(resolvedPath) {
  const marker = `${path.sep}internal${path.sep}`;
  const index = resolvedPath.indexOf(marker);
  if (index === -1) return null;

  const beforeInternal = resolvedPath.slice(0, index);
  const srcIndex = beforeInternal.lastIndexOf(`${path.sep}src${path.sep}`);
  if (srcIndex === -1) return beforeInternal;

  const afterSrc = beforeInternal.slice(srcIndex + 5);
  const firstSegment = afterSrc.split(path.sep)[0];
  return firstSegment
    ? path.join(beforeInternal.slice(0, srcIndex + 5), firstSegment)
    : beforeInternal;
}

async function isDirectory(filePath) {
  try {
    return (await stat(filePath)).isDirectory();
  } catch (error) {
    if (error && error.code === "ENOENT") return false;
    throw error;
  }
}

for (const root of roots) {
  if (!(await isDirectory(root))) continue;
  for await (const file of walk(root)) {
    const source = await readFile(file, "utf8");
    for (const match of source.matchAll(importPattern)) {
      const specifier = match[1];
      if (!specifier) continue;

      if (!specifier.startsWith(".") && specifier.includes("/internal")) {
        errors.push(`${file}: imports internal module '${specifier}'`);
        continue;
      }

      if (!specifier.startsWith(".")) continue;
      const resolved = path.normalize(path.join(path.dirname(file), specifier));
      const internalRoot = moduleRootForInternal(`${resolved}${path.sep}`);
      if (internalRoot && !path.normalize(file).startsWith(internalRoot)) {
        errors.push(`${file}: crosses into internal module '${specifier}'`);
      }
    }
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}
