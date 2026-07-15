import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const pkg = JSON.parse(await readFile("package.json", "utf8"));
const nodeVersion = (await readFile(".node-version", "utf8")).trim();
const rustToolchain = await readFile("rust-toolchain.toml", "utf8");
const verifyWorkflow = await readFile(".github/workflows/verify.yml", "utf8");
let releaseWorkflow = null;
try {
  releaseWorkflow = await readFile(".github/workflows/release.yml", "utf8");
} catch (error) {
  if (error?.code !== "ENOENT") throw error;
}

assert.equal(nodeVersion, "24", ".node-version must pin Node 24");
assert.equal(
  pkg.engines?.node,
  "24.x",
  "package.json engines.node must pin Node 24.x",
);
assert.equal(
  pkg.packageManager,
  "pnpm@10.30.0",
  "packageManager must pin pnpm 10.30.0",
);
assert.match(
  rustToolchain,
  /^channel = "1\.97\.0"$/mu,
  "rust-toolchain.toml must pin Rust 1.97.0",
);
assert.match(
  verifyWorkflow,
  /^\s*- uses: dtolnay\/rust-toolchain@1\.97\.0$/mu,
  "verification must use the repository-pinned Rust 1.97.0 toolchain",
);
if (releaseWorkflow !== null) {
  assert.match(
    releaseWorkflow,
    /^\s*- uses: dtolnay\/rust-toolchain@1\.97\.0$/mu,
    "release binaries must use the repository-pinned Rust 1.97.0 toolchain",
  );
}

console.log("toolchain declarations are pinned");
