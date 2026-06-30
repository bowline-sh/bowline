import { execFileSync, spawnSync } from "node:child_process";
import path from "node:path";

function parseArgs(argv) {
  const args = {
    push: false,
    target: null,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--push") {
      args.push = true;
    } else if (arg === "--target") {
      args.target = argv[++index] ?? null;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.target) throw new Error("--target requires a public repo path");
  return args;
}

function git(root, args) {
  return execFileSync("git", ["-C", root, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? process.cwd(),
    encoding: "utf8",
    stdio: "inherit",
  });

  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed`);
  }
}

function assertCleanRepo(root, label) {
  git(root, ["rev-parse", "--git-dir"]);
  const status = git(root, ["status", "--porcelain"]);
  if (status.length > 0) {
    throw new Error(`${label} repo has uncommitted changes`);
  }
}

function targetHasChanges(targetRoot) {
  return git(targetRoot, ["status", "--porcelain"]).length > 0;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const sourceRoot = process.cwd();
  const targetRoot = path.resolve(args.target);

  assertCleanRepo(sourceRoot, "Source");
  assertCleanRepo(targetRoot, "Target");

  run(process.execPath, ["scripts/export-public.mjs", "--target", targetRoot]);
  run(process.execPath, [
    "scripts/check-public-export.mjs",
    "--root",
    targetRoot,
  ]);

  const hasChanges = targetHasChanges(targetRoot);

  run("pnpm", ["install", "--frozen-lockfile"], { cwd: targetRoot });
  if (hasChanges) git(targetRoot, ["add", "-A"]);
  run("pnpm", ["verify:public"], { cwd: targetRoot });

  if (!hasChanges) {
    console.log("Public export is unchanged; no commit created.");
    return;
  }

  const sourceSha = git(sourceRoot, ["rev-parse", "HEAD"]);
  git(targetRoot, [
    "commit",
    "-m",
    `chore: sync public export from ${sourceSha}`,
  ]);

  if (args.push) {
    git(targetRoot, ["push"]);
  } else {
    console.log(
      "Created public export commit locally. Re-run with --push to publish it.",
    );
  }
}

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
