import { readFile, readdir } from "node:fs/promises";
import path from "node:path";

const errors = [];
const root = parseRoot(process.argv.slice(2));

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(fullPath);
    } else if (entry.name.endsWith(".rs")) {
      yield fullPath;
    }
  }
}

for await (const file of walk(root)) {
  const source = await readFile(file, "utf8");
  if (/\bpub\s+mod\s+internal\b/.test(source)) {
    errors.push(`${file}: do not expose internal modules publicly`);
  }
  if (/\bpub\s+use\s+internal::/.test(source)) {
    errors.push(`${file}: do not re-export internal modules`);
  }
  if (importsRawConvex(source) && !isHostedControlPlaneAdapter(file)) {
    errors.push(
      `${file}: raw Convex imports must stay inside crates/bowline-control-plane/src/hosted.rs`,
    );
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}

function importsRawConvex(source) {
  return (
    /\buse\s+convex(::|\s*;)/.test(source) ||
    /\bconvex::/.test(source) ||
    /\bextern\s+crate\s+convex\b/.test(source)
  );
}

function isHostedControlPlaneAdapter(file) {
  return (
    path.normalize(file) ===
    path.join(root, "bowline-control-plane", "src", "hosted.rs")
  );
}

function parseRoot(args) {
  if (args.length === 0) return "crates";
  if (args.length === 2 && args[0] === "--root") return args[1];
  console.error("usage: check-rust-boundaries.mjs [--root <dir>]");
  process.exit(2);
}
