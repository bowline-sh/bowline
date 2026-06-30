import { spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const expectedFailures = [
  {
    args: [
      "scripts/check-generated-artifacts.mjs",
      "--files-from",
      "tests/fixtures/generated-artifacts/tracked-files.txt",
    ],
    name: "generated artifact fixture",
    output: "tracked generated artifact",
  },
  {
    args: [
      "scripts/check-architecture-imports.mjs",
      "--root",
      "tests/fixtures/architecture-imports",
    ],
    name: "internal import fixture",
    output: "crosses into internal module",
  },
  {
    args: [
      "scripts/check-rust-boundaries.mjs",
      "--root",
      "tests/fixtures/rust-boundaries",
    ],
    name: "rust convex boundary fixture",
    output: "raw Convex imports must stay",
  },
  {
    args: [
      "scripts/check-public-export.mjs",
      "--root",
      "tests/fixtures/public-export/content",
      "--files-from",
      "tests/fixtures/public-export/leaky-files.txt",
    ],
    name: "public export leak fixture",
    output: "must stay private",
  },
];

const errors = [];

for (const fixture of expectedFailures) {
  const result = spawnSync(process.execPath, fixture.args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });

  if (result.status === 0) {
    errors.push(`${fixture.name}: expected failure but command succeeded`);
    continue;
  }

  const output = `${result.stdout}\n${result.stderr}`;
  if (!output.includes(fixture.output)) {
    errors.push(
      `${fixture.name}: expected output containing '${fixture.output}'`,
    );
  }
}

function run(command, args, options = {}) {
  return spawnSync(command, args, {
    cwd: options.cwd ?? process.cwd(),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function requireSuccess(name, result) {
  if (result.status !== 0) {
    errors.push(
      `${name}: expected success but failed\n${result.stdout}\n${result.stderr}`,
    );
  }
}

function requireFailure(name, result, output) {
  if (result.status === 0) {
    errors.push(`${name}: expected failure but command succeeded`);
    return;
  }

  const combined = `${result.stdout}\n${result.stderr}`;
  if (!combined.includes(output)) {
    errors.push(`${name}: expected output containing '${output}'`);
  }
}

function runPublicExportFixture() {
  const root = mkdtempSync(join(tmpdir(), "bowline-public-export-"));
  const source = join(root, "source");
  const target = join(root, "target");

  mkdirSync(join(source, "docs"), { recursive: true });
  mkdirSync(join(source, "leaky"), { recursive: true });
  mkdirSync(target, { recursive: true });
  writeFileSync(join(source, "README.md"), "# Fixture\n");
  writeFileSync(join(source, "docs", "public.md"), "public\n");
  writeFileSync(
    join(source, "leaky", ".env.local"),
    "TOKEN=fixture-token-value\n",
  );
  writeFileSync(join(source, "run.sh"), "#!/usr/bin/env bash\nexit 0\n");
  chmodSync(join(source, "run.sh"), 0o755);
  writeFileSync(
    join(source, "public-export.fixture.json"),
    JSON.stringify({ include: ["README.md", "docs", "run.sh"] }, null, 2),
  );

  requireSuccess(
    "public export fixture git init",
    run("git", ["init"], { cwd: target }),
  );
  requireSuccess(
    "public export fixture git config name",
    run("git", ["config", "user.name", "fixture"], { cwd: target }),
  );
  requireSuccess(
    "public export fixture git config email",
    run("git", ["config", "user.email", "fixture@example.com"], {
      cwd: target,
    }),
  );
  writeFileSync(join(target, "stale.txt"), "stale\n");
  requireSuccess(
    "public export fixture git add",
    run("git", ["add", "stale.txt"], { cwd: target }),
  );
  requireSuccess(
    "public export fixture git commit",
    run("git", ["commit", "-m", "seed"], { cwd: target }),
  );

  requireSuccess(
    "public export fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/export-public.mjs"),
      "--source",
      source,
      "--manifest",
      "public-export.fixture.json",
      "--target",
      target,
    ]),
  );

  if (!existsSync(join(target, "README.md"))) {
    errors.push("public export fixture: README.md was not exported");
  }
  if (!existsSync(join(target, "docs", "public.md"))) {
    errors.push("public export fixture: docs/public.md was not exported");
  }
  if (existsSync(join(target, "stale.txt"))) {
    errors.push("public export fixture: stale file was not pruned");
  }

  writeFileSync(
    join(source, "public-export.leaky.json"),
    JSON.stringify({ include: ["leaky"] }, null, 2),
  );
  requireFailure(
    "public export manifest leak fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/check-public-export.mjs"),
      "--root",
      source,
      "--manifest",
      "public-export.leaky.json",
    ]),
    "raw env files must stay private",
  );
  const secretRoot = join(root, "secret-content");
  mkdirSync(secretRoot);
  writeFileSync(
    join(secretRoot, "fake-token.txt"),
    "PUBLIC_EXPORT_TEST_TOKEN=not-a-real-token-value\n",
  );
  writeFileSync(join(secretRoot, "files.txt"), "fake-token.txt\n");
  requireFailure(
    "public export secret fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/check-public-export.mjs"),
      "--root",
      secretRoot,
      "--files-from",
      join(secretRoot, "files.txt"),
    ]),
    "secret-looking assignment",
  );
  writeFileSync(
    join(secretRoot, "home-path.txt"),
    ["", "Users", "alice", "Code", "acme"].join("/") + "\n",
  );
  writeFileSync(join(secretRoot, "home-files.txt"), "home-path.txt\n");
  requireFailure(
    "public export home path fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/check-public-export.mjs"),
      "--root",
      secretRoot,
      "--files-from",
      join(secretRoot, "home-files.txt"),
    ]),
    "private local absolute path",
  );
  symlinkSync("../private-source", join(source, "private-link"));
  writeFileSync(
    join(source, "public-export.symlink.json"),
    JSON.stringify({ include: ["private-link"] }, null, 2),
  );
  const symlinkTarget = join(root, "symlink-target");
  mkdirSync(symlinkTarget);
  requireSuccess(
    "public export symlink fixture git init",
    run("git", ["init"], { cwd: symlinkTarget }),
  );
  requireSuccess(
    "public export symlink fixture git config name",
    run("git", ["config", "user.name", "fixture"], { cwd: symlinkTarget }),
  );
  requireSuccess(
    "public export symlink fixture git config email",
    run("git", ["config", "user.email", "fixture@example.com"], {
      cwd: symlinkTarget,
    }),
  );
  writeFileSync(join(symlinkTarget, "seed.txt"), "keep\n");
  requireSuccess(
    "public export symlink fixture git add",
    run("git", ["add", "seed.txt"], { cwd: symlinkTarget }),
  );
  requireSuccess(
    "public export symlink fixture git commit",
    run("git", ["commit", "-m", "seed"], { cwd: symlinkTarget }),
  );
  requireFailure(
    "public export symlink fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/export-public.mjs"),
      "--source",
      source,
      "--manifest",
      "public-export.symlink.json",
      "--target",
      symlinkTarget,
    ]),
    "Symlinks are not exported",
  );
  if (!existsSync(join(symlinkTarget, "seed.txt"))) {
    errors.push(
      "public export symlink fixture: target was pruned before validation",
    );
  }
  writeFileSync(join(target, "AGENTS.md"), "private\n");
  requireSuccess(
    "public all-tracked leak fixture git add",
    run("git", ["add", "AGENTS.md"], { cwd: target }),
  );
  requireFailure(
    "public all-tracked leak fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/check-public-export.mjs"),
      "--root",
      target,
      "--all-tracked",
    ]),
    "prerelease agent instructions must stay private",
  );

  const dirtyTarget = join(root, "dirty-target");
  mkdirSync(dirtyTarget);
  requireSuccess(
    "public export dirty fixture git init",
    run("git", ["init"], { cwd: dirtyTarget }),
  );
  writeFileSync(join(dirtyTarget, "dirty.txt"), "dirty\n");
  requireFailure(
    "public export dirty target fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/export-public.mjs"),
      "--source",
      source,
      "--manifest",
      "public-export.fixture.json",
      "--target",
      dirtyTarget,
    ]),
    "uncommitted changes",
  );
  const ancestorTarget = join(root, "ancestor-target");
  const ancestorSource = join(ancestorTarget, "source");
  mkdirSync(ancestorSource, { recursive: true });
  requireSuccess(
    "public export ancestor fixture git init",
    run("git", ["init"], { cwd: ancestorTarget }),
  );
  requireFailure(
    "public export ancestor target fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/export-public.mjs"),
      "--source",
      ancestorSource,
      "--target",
      ancestorTarget,
    ]),
    "must not contain the private source repo",
  );
  const symlinkedTargetSource = join(root, "symlinked-target-source");
  const symlinkedTargetPath = join(root, "target-link");
  mkdirSync(symlinkedTargetSource);
  writeFileSync(join(symlinkedTargetSource, "README.md"), "# Source\n");
  requireSuccess(
    "public export symlinked target fixture git init",
    run("git", ["init"], { cwd: symlinkedTargetSource }),
  );
  symlinkSync(symlinkedTargetSource, symlinkedTargetPath);
  requireFailure(
    "public export symlinked target fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/export-public.mjs"),
      "--source",
      symlinkedTargetSource,
      "--target",
      symlinkedTargetPath,
    ]),
    "outside the private source repo",
  );
  if (!existsSync(join(symlinkedTargetSource, "README.md"))) {
    errors.push("public export symlinked target fixture: source was pruned");
  }
  requireSuccess(
    "missing architecture root fixture",
    run(process.execPath, [
      join(process.cwd(), "scripts/check-architecture-imports.mjs"),
      "--root",
      join(root, "missing-root"),
    ]),
  );

  rmSync(root, { recursive: true, force: true });
}

runPublicExportFixture();

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}
