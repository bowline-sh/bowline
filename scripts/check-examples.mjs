import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { readdir } from "node:fs/promises";
import path from "node:path";

const examplesRoot = "examples/merge-plugins";
const errors = [];
const targetDir = process.env.CARGO_TARGET_DIR ?? "/tmp/bowline-dev-target";

for (const entry of await readdir(examplesRoot, { withFileTypes: true })) {
  if (!entry.isDirectory()) continue;

  const manifest = path.join(examplesRoot, entry.name, "Cargo.toml");
  if (!existsSync(manifest)) continue;

  try {
    execFileSync("cargo", ["build", "--manifest-path", manifest], {
      stdio: "inherit",
      env: { ...process.env, CARGO_TARGET_DIR: targetDir },
    });
  } catch {
    errors.push(`${manifest}: cargo build failed`);
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}
