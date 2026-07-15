import { execFileSync } from "node:child_process";
import { realpathSync } from "node:fs";
import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const UNAVAILABLE_EXIT_CODE = 2;
const USAGE_EXIT_CODE = 3;

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

export async function checkRustSources(root) {
  const errors = [];
  for await (const file of walk(root)) {
    const source = await readFile(file, "utf8");
    if (/\bpub\s+mod\s+internal\b/.test(source)) {
      errors.push(`${file}: do not expose internal modules publicly`);
    }
    if (/\bpub\s+use\s+internal::/.test(source)) {
      errors.push(`${file}: do not re-export internal modules`);
    }
    if (importsRawConvex(source) && !isHostedControlPlaneAdapter(file, root)) {
      errors.push(
        `${file}: raw Convex imports must stay inside crates/bowline-control-plane/src/hosted.rs`,
      );
    }
    if (
      directlyComposesDaemonStatus(source) &&
      isDaemonSource(file) &&
      !isStatusProjectionOwner(file)
    ) {
      errors.push(
        `${file}: daemon status composition must stay inside status_projection`,
      );
    }
    if (isDaemonEventLoop(file) && hasLegacyDaemonPolling(source)) {
      errors.push(
        `${file}: daemon accept/coordinator loops must be wakeable and must not restore fixed short polling or per-connection thread spawning`,
      );
    }
    if (isDaemonConnectionPump(file) && hasDirectRequestWorkerSpawn(source)) {
      errors.push(
        `${file}: daemon connection pumps must dispatch through bounded executors, never spawn a worker per request`,
      );
    }
    if (isDaemonThreadOwner(file) && hasDetachFallback(source)) {
      errors.push(
        `${file}: daemon shutdown must join owned threads or report forced recovery, never detach a live worker`,
      );
    }
  }
  return errors;
}

function directlyComposesDaemonStatus(source) {
  return /\b(?:compose_status|RevisionedStatusComposer)\b/.test(source);
}

function isDaemonSource(file) {
  return path.normalize(file).split(path.sep).includes("bowline-daemon");
}

function isStatusProjectionOwner(file) {
  const parts = path.normalize(file).split(path.sep);
  const daemon = parts.lastIndexOf("bowline-daemon");
  return (
    daemon >= 0 &&
    parts[daemon + 1] === "src" &&
    parts[daemon + 2] === "status_projection"
  );
}

function isDaemonEventLoop(file) {
  const normalized = path.normalize(file);
  if (normalized.endsWith(`${path.sep}tests.rs`)) return false;
  return [
    path.join("bowline-daemon", "src", "daemon", "coordinator.rs"),
    path.join("bowline-daemon", "src", "daemon", "coordinator", "lanes.rs"),
    path.join("bowline-daemon", "src", "daemon", "protocol.rs"),
    path.join("bowline-daemon", "src", "daemon", "protocol", "acceptor.rs"),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol",
      "coordinator_runtime.rs",
    ),
    path.join("bowline-daemon", "src", "daemon", "sync", "scheduler_poll.rs"),
    path.join("bowline-daemon", "src", "status_projection", "service.rs"),
  ].some((suffix) => normalized.endsWith(suffix));
}

function hasLegacyDaemonPolling(source) {
  const directShortWait =
    /\b(?:sleep|park_timeout|recv_timeout|wait_timeout)\s*\(\s*Duration::from_millis\(\s*([0-9]+)\s*\)\s*\)/gu;
  for (const match of source.matchAll(directShortWait)) {
    if (Number(match[1]) < 1000) return true;
  }
  const shortDurationConstants = new Set();
  const durationConstant =
    /\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*Duration\s*=\s*Duration::from_millis\(\s*([0-9]+)\s*\)/gu;
  for (const match of source.matchAll(durationConstant)) {
    if (Number(match[2]) < 1000) shortDurationConstants.add(match[1]);
  }
  const waitsOnShortConstant = [...shortDurationConstants].some((name) =>
    new RegExp(
      `\\b(?:sleep|park_timeout|recv_timeout|wait_timeout)\\s*\\(\\s*${name}\\s*\\)`,
      "u",
    ).test(source),
  );
  return (
    waitsOnShortConstant ||
    hasShortDurationAliasWait(source) ||
    /\bcrossbeam_channel::(?:after|tick)\s*\(\s*Duration::from_millis\(\s*(?:[0-9]|[1-9][0-9]{1,2})\s*\)\s*\)/u.test(
      source,
    ) ||
    /\bthread_sleep_short\b/.test(source) ||
    /listener\.set_nonblocking\(true\)/.test(source)
  );
}

function hasShortDurationAliasWait(source) {
  const aliases = new Set();
  const aliasPattern =
    /\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)\s*=\s*Duration::from_millis\(\s*([0-9]+)\s*\)/gu;
  for (const match of source.matchAll(aliasPattern)) {
    if (Number(match[2]) < 1000) aliases.add(match[1]);
  }
  return [...aliases].some((name) =>
    new RegExp(
      `\\b(?:sleep|park_timeout|recv_timeout|wait_timeout)\\s*\\(\\s*${name}\\s*\\)`,
      "u",
    ).test(source),
  );
}

function isDaemonConnectionPump(file) {
  return path
    .normalize(file)
    .endsWith(
      path.join(
        "bowline-daemon",
        "src",
        "daemon",
        "protocol_v2",
        "connection_pump.rs",
      ),
    );
}

function hasDirectRequestWorkerSpawn(source) {
  return (
    /\b(?:std::)?thread::spawn\s*\(/.test(source) ||
    /\b(?:std::)?thread::Builder::new\(\)[\s\S]{0,500}\.spawn\(/.test(source)
  );
}

function isDaemonThreadOwner(file) {
  const normalized = path.normalize(file);
  return [
    path.join("bowline-daemon", "src", "daemon", "protocol.rs"),
    path.join("bowline-daemon", "src", "daemon", "coordinator.rs"),
    path.join("bowline-daemon", "src", "daemon", "coordinator", "lanes.rs"),
    path.join("bowline-daemon", "src", "daemon", "protocol", "acceptor.rs"),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol",
      "coordinator_runtime.rs",
    ),
    path.join("bowline-daemon", "src", "daemon", "protocol", "supervisor.rs"),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol",
      "connection_executor.rs",
    ),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol_v2",
      "rpc_executor.rs",
    ),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol_v2",
      "connection_pump.rs",
    ),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "protocol_v2",
      "connection_pump",
      "reader.rs",
    ),
    path.join("bowline-daemon", "src", "status_projection", "service.rs"),
    path.join("bowline-daemon", "src", "daemon", "sync", "executor.rs"),
    path.join(
      "bowline-daemon",
      "src",
      "daemon",
      "sync",
      "executor",
      "remote_observer.rs",
    ),
  ].some((suffix) => normalized.endsWith(suffix));
}

function hasDetachFallback(source) {
  return (
    /\blet\s+mut\s+detached(?:_\w+)?\b/.test(source) ||
    /\b(?:std::)?mem::forget\s*\(/.test(source) ||
    /detached\s+(?:a\s+)?blocked\s+(?:sync\s+)?(?:scheduler|worker)/.test(
      source,
    )
  );
}

function importsRawConvex(source) {
  return (
    /\buse\s+convex(::|\s*;)/.test(source) ||
    /\bconvex::/.test(source) ||
    /\bextern\s+crate\s+convex\b/.test(source)
  );
}

function isHostedControlPlaneAdapter(file, root) {
  return (
    path.normalize(file) ===
    path.join(root, "bowline-control-plane", "src", "hosted.rs")
  );
}

export function parseArgs(args) {
  if (args.length === 0) {
    return {
      root: "crates",
      source: true,
      cargoMetadata: true,
      selfTests: true,
    };
  }
  if (args.length === 1 && args[0] === "--cargo-only") {
    return {
      root: "crates",
      source: false,
      cargoMetadata: true,
      selfTests: true,
    };
  }
  if (
    args.length === 3 &&
    args[0] === "--source-only" &&
    args[1] === "--root" &&
    args[2].length > 0
  ) {
    return {
      root: args[2],
      source: true,
      cargoMetadata: false,
      selfTests: false,
    };
  }
  throw new Error(
    "usage: check-rust-boundaries.mjs [--cargo-only | --source-only --root <dir>]",
  );
}

export function checkCargoMetadata() {
  const metadata = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version=1", "--no-deps"], {
      cwd: process.cwd(),
      encoding: "utf8",
      env: { ...process.env, CARGO_TARGET_DIR: "/tmp/bowline-dev-target" },
    }),
  );
  const workspaceMembers = new Set(metadata.workspace_members);
  return metadata.packages
    .filter((pkg) => workspaceMembers.has(pkg.id))
    .flatMap(checkCargoMetadataPackage);
}

function checkCargoMetadataPackage(pkg) {
  const packageErrors = [];
  const manifestPath = path.relative(process.cwd(), pkg.manifest_path);
  for (const dependency of pkg.dependencies) {
    if (dependency.name === "bowline-testkit" && dependency.kind !== "dev") {
      packageErrors.push(
        `${manifestPath}: bowline-testkit may only be used from [dev-dependencies]`,
      );
    }
    if (
      dependency.kind !== "dev" &&
      dependency.features.includes("fault-injection") &&
      !faultFeatureOwner(pkg.name)
    ) {
      packageErrors.push(
        `${manifestPath}: fault-injection must not be enabled from shipping dependency or feature paths`,
      );
    }
  }
  if (!faultFeatureOwner(pkg.name)) {
    for (const values of Object.values(pkg.features ?? {})) {
      if (values.some((value) => value.includes("fault-injection"))) {
        packageErrors.push(
          `${manifestPath}: fault-injection must not be enabled from shipping dependency or feature paths`,
        );
      }
    }
  }
  return packageErrors;
}

function faultFeatureOwner(crateName) {
  return crateName === "bowline-local" || crateName === "bowline-testkit";
}

export function selfTestCargoManifestChecks() {
  const failures = [];
  assertCheckFails(
    failures,
    "synthetic-shipping-testkit/Cargo.toml",
    '[package]\nname = "bad"\n[dependencies]\nbowline-testkit = { path = "../bowline-testkit" }\n',
    "bowline-testkit may only be used",
  );
  assertCheckFails(
    failures,
    "synthetic-shipping-fault/Cargo.toml",
    '[package]\nname = "bad"\n[dependencies]\nbowline-local = { path = "../bowline-local", features = ["fault-injection"] }\n',
    "fault-injection must not be enabled",
  );
  assertCheckPasses(
    failures,
    "synthetic-dev-testkit/Cargo.toml",
    '[package]\nname = "ok"\n[dev-dependencies]\nbowline-testkit = { path = "../bowline-testkit" }\n',
  );
  assertCheckPasses(
    failures,
    "synthetic-testkit-forward/Cargo.toml",
    '[package]\nname = "bowline-testkit"\n[features]\nfault-injection = ["bowline-local/fault-injection"]\n',
  );
  return failures;
}

function assertCheckFails(failures, file, source, expected) {
  const found = checkSyntheticManifest(file, source).some((error) =>
    error.includes(expected),
  );
  if (!found) {
    failures.push(
      `${file}: rust boundary self-test did not reject ${expected}`,
    );
  }
}

function assertCheckPasses(failures, file, source) {
  const result = checkSyntheticManifest(file, source);
  if (result.length > 0) {
    failures.push(
      `${file}: rust boundary self-test unexpectedly failed: ${result.join("; ")}`,
    );
  }
}

function checkSyntheticManifest(file, source) {
  const pkg = syntheticPackage(file, source);
  return checkCargoMetadataPackage(pkg);
}

function syntheticPackage(file, source) {
  const name = source.match(/^\s*name\s*=\s*"([^"]+)"/m)?.[1] ?? "synthetic";
  const dependencies = [];
  if (source.includes("[dependencies]")) {
    if (source.includes("bowline-testkit")) {
      dependencies.push(syntheticDependency("bowline-testkit", null, []));
    }
    if (source.includes('features = ["fault-injection"]')) {
      dependencies.push(
        syntheticDependency("bowline-local", null, ["fault-injection"]),
      );
    }
  }
  if (
    source.includes("[dev-dependencies]") &&
    source.includes("bowline-testkit")
  ) {
    dependencies.push(syntheticDependency("bowline-testkit", "dev", []));
  }
  const features = source.includes(
    '[features]\nfault-injection = ["bowline-local/fault-injection"]',
  )
    ? { "fault-injection": ["bowline-local/fault-injection"] }
    : {};
  return {
    dependencies,
    features,
    manifest_path: path.join(process.cwd(), file),
    name,
  };
}

function syntheticDependency(name, kind, features) {
  return {
    features,
    kind,
    name,
    uses_default_features: true,
  };
}

export async function run(args, overrides = {}) {
  let options;
  try {
    options = parseArgs(args);
  } catch (error) {
    console.error(error.message);
    return USAGE_EXIT_CODE;
  }
  options.root = overrides.sourceRoot ?? options.root;
  const errors = options.source ? await checkRustSources(options.root) : [];
  if (options.cargoMetadata) {
    try {
      errors.push(...checkCargoMetadata());
    } catch (error) {
      if (error?.code === "ENOENT") {
        if (errors.length > 0) {
          console.error(errors.join("\n"));
          return 1;
        }
        console.error(
          "check-rust-boundaries: required tool unavailable: cargo",
        );
        return UNAVAILABLE_EXIT_CODE;
      }
      throw error;
    }
  }
  if (options.selfTests) errors.push(...selfTestCargoManifestChecks());
  if (errors.length > 0) {
    console.error(errors.join("\n"));
    return 1;
  }
  return 0;
}

if (
  process.argv[1] &&
  realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))
) {
  run(process.argv.slice(2))
    .then((exitCode) => {
      process.exitCode = exitCode;
    })
    .catch((error) => {
      console.error(error.message);
      process.exitCode = 1;
    });
}
