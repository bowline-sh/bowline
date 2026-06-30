#!/usr/bin/env node
import { spawnSync } from "node:child_process";

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: options.optional ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.status !== 0) {
    if (options.optional && result.error?.code === "ENOENT") return false;
    throw new Error(`${command} ${args.join(" ")} failed`);
  }
  return true;
}

run("sh", ["-n", "scripts/install.sh"]);

const hasShellcheck = run("shellcheck", ["--version"], { optional: true });
if (hasShellcheck) {
  run("shellcheck", ["scripts/install.sh"]);
} else {
  console.error("shellcheck not found; skipped install script lint");
}
