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
    const pkg = JSON.parse(await readFile(packagePath, "utf8"));
    for (const script of required) {
      if (typeof pkg.scripts?.[script] !== "string") {
        missing.push(`${packagePath}: missing scripts.${script}`);
      }
    }
  }
}

if (missing.length > 0) {
  console.error(missing.join("\n"));
  process.exit(1);
}
