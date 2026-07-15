import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";

const DEFAULT_ROOTS = ["apps", "packages", "crates"];
const PHASE_ZERO_ALLOWLIST = new Set([
  path.normalize("crates/bowline-testkit/examples/namespace_baseline.rs"),
  path.normalize("crates/bowline-local/src/page_test_support.rs"),
]);
const ARCHITECTURE_FIXTURE_ROOT = path.normalize(
  "tests/fixtures/architecture-imports",
);
const TEST_DIRECTORY_NAMES = new Set([
  "__tests__",
  "fixtures",
  "test",
  "tests",
]);
const importPattern =
  /\b(?:import|export)\s+(?:type\s+)?(?:[^'"]*?\s+from\s+)?["']([^"']+)["']/g;
const cutoverRules = [
  {
    id: "flat-manifest-entries",
    message:
      "production namespace code must use the page reader instead of SnapshotManifest.entries",
    pattern:
      /\b(?:manifest|[A-Za-z_][A-Za-z0-9_]*manifest)(?:\s*\(\s*\))?\s*\.\s*entries\b/gi,
  },
  {
    id: "flat-namespace-adapter",
    message: "FlatNamespaceReader and FlatNamespaceBuilder are test-only",
    pattern: /\bFlatNamespace(?:Reader|Builder)\b/g,
  },
  {
    id: "flat-manifest-sql",
    message: "snapshot persistence must not store manifest_json",
    pattern: /\bmanifest_json\b/g,
  },
  {
    id: "manifest-chunk-cache",
    message: "page nodes replace the manifest chunk cache",
    pattern: /\b(?:manifest_chunk_cache|manifestChunkCache)\b/g,
  },
  {
    id: "snapshot-pack-array",
    message:
      "snapshot-wide pack arrays must be replaced by bounded reachability records",
    pattern: /\b(?:packObjectKeys|packObjects|pack_objects)\b/g,
  },
  {
    id: "flat-snapshot-collection-helper",
    message:
      "production namespace paths must stream readers instead of collecting snapshot entries",
    pattern: /\bsnapshot_entries\s*\(/g,
  },
  {
    id: "whole-page-graph-collection",
    message:
      "production page persistence must stream metadata records instead of collecting a whole graph",
    pattern: /\.\s*(?:plaintext_records|reachable_plaintext_records)\s*\(/g,
  },
];
const flatManifestCodecPattern = /\b(?:seal|open)_snapshot_manifest\s*\(/g;
const flatAuthorityPattern =
  /\b(?:manifest|[A-Za-z_][A-Za-z0-9_]*manifest)(?:\s*\(\s*\))?\s*\.\s*entries\b|\bmanifest_json\b|\bmanifest_chunk_cache\b/i;

function parseRoots(argv) {
  const roots = [];
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] !== "--root") continue;
    const root = argv[index + 1];
    if (!root) {
      console.error("--root requires a path");
      process.exit(2);
    }
    roots.push(root);
    index += 1;
  }
  return roots.length > 0 ? roots : DEFAULT_ROOTS;
}

async function* walk(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (["dist", "node_modules", "target"].includes(entry.name)) continue;
    const fullPath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      yield* walk(fullPath);
    } else if (/\.(?:rs|ts|tsx|mts|cts)$/.test(entry.name)) {
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

function isTestOnlyPath(file) {
  const projectRelative = path.relative(process.cwd(), path.resolve(file));
  const normalized = projectRelative.startsWith(
    `${ARCHITECTURE_FIXTURE_ROOT}${path.sep}`,
  )
    ? projectRelative.slice(ARCHITECTURE_FIXTURE_ROOT.length + 1)
    : projectRelative;
  const segments = normalized.split(path.sep);
  if (
    segments.some(
      (segment) =>
        TEST_DIRECTORY_NAMES.has(segment) ||
        segment.endsWith("-tests") ||
        segment.endsWith("_tests"),
    )
  ) {
    return true;
  }
  return /(?:^|[_.-])tests?\.(?:rs|ts|tsx|mts|cts)$/.test(path.basename(file));
}

function maskRustTestRegions(source) {
  const lines = source.split("\n");
  let pendingTestItem = false;
  let masking = false;
  let braceDepth = 0;

  return lines
    .map((line) => {
      if (
        !pendingTestItem &&
        !masking &&
        /^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]/.test(line)
      ) {
        pendingTestItem = true;
        return "";
      }
      if (!pendingTestItem && !masking) return line;

      const opens = (line.match(/{/g) ?? []).length;
      const closes = (line.match(/}/g) ?? []).length;
      braceDepth += opens - closes;
      if (pendingTestItem && opens > 0) {
        pendingTestItem = false;
        masking = true;
      } else if (pendingTestItem && line.includes(";")) {
        pendingTestItem = false;
      }
      if (masking && braceDepth <= 0) {
        masking = false;
        braceDepth = 0;
      }
      return "";
    })
    .join("\n");
}

function productionSource(file, source) {
  const normalized = path.relative(process.cwd(), path.resolve(file));
  if (PHASE_ZERO_ALLOWLIST.has(normalized) || isTestOnlyPath(file)) {
    return "";
  }
  return normalized.endsWith(".rs") ? maskRustTestRegions(source) : source;
}

function lineNumberAt(source, index) {
  let line = 1;
  for (let offset = 0; offset < index; offset += 1) {
    if (source.charCodeAt(offset) === 10) line += 1;
  }
  return line;
}

function cutoverErrors(file, source) {
  const production = productionSource(file, source);
  const errors = [];
  for (const rule of cutoverRules) {
    rule.pattern.lastIndex = 0;
    for (const match of production.matchAll(rule.pattern)) {
      errors.push(
        `${file}:${lineNumberAt(production, match.index)}: ${rule.message} [${rule.id}]`,
      );
    }
  }
  if (flatAuthorityPattern.test(production)) {
    flatManifestCodecPattern.lastIndex = 0;
    for (const match of production.matchAll(flatManifestCodecPattern)) {
      errors.push(
        `${file}:${lineNumberAt(production, match.index)}: old flat snapshot-manifest seal/open calls are forbidden [flat-manifest-codec]`,
      );
    }
  }
  return errors;
}

function importErrors(file, source) {
  if (!/\.(?:ts|tsx|mts|cts)$/.test(file)) return [];
  const errors = [];
  importPattern.lastIndex = 0;
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
  return errors;
}

const roots = parseRoots(process.argv.slice(2));
const errors = [];

for (const root of roots) {
  if (!(await isDirectory(root))) continue;
  for await (const file of walk(root)) {
    const source = await readFile(file, "utf8");
    errors.push(...importErrors(file, source));
    errors.push(...cutoverErrors(file, source));
  }
}

if (errors.length > 0) {
  const uniqueErrors = [...new Set(errors)].sort();
  console.error(uniqueErrors.join("\n"));
  process.exit(1);
}
