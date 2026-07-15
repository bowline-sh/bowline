import { existsSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

const roots = ["apps", "packages"];
const required = ["build", "test", "typecheck"];
const missing = [];

for (const root of roots) {
  let entries = [];
  try {
    entries = await readdir(root, { withFileTypes: true });
  } catch {
    continue;
  }

  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const packagePath = path.join(root, entry.name, "package.json");
    if (!existsSync(packagePath)) continue;
    const pkg = JSON.parse(await readFile(packagePath, "utf8"));
    for (const script of required) {
      if (typeof pkg.scripts?.[script] !== "string") {
        missing.push(`${packagePath}: missing scripts.${script}`);
      }
    }
  }
}

const rootPkg = JSON.parse(await readFile("package.json", "utf8"));
const scriptRef = /\b(?:node|bash)\s+(scripts\/[\w./-]+\.(?:mjs|sh))\b/g;
for (const [name, command] of Object.entries(rootPkg.scripts ?? {})) {
  if (typeof command !== "string") continue;
  for (const match of command.matchAll(scriptRef)) {
    const file = match[1];
    if (!existsSync(file)) {
      missing.push(
        `package.json: scripts.${name} references missing file ${file}`,
      );
    }
  }
}

if (missing.length > 0) {
  console.error(missing.join("\n"));
  process.exit(1);
}
