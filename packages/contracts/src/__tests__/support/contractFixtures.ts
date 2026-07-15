import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

export type ContractManifest = {
  manifestVersion: number;
  fixtures: ContractManifestFixture[];
};

export type ContractManifestFixture = {
  id: string;
  family: string;
  path: string;
  format: string;
  kind: string;
  languageDecoders: {
    typescript?: string;
  };
};

export function readContractFixture(relativePath: string): unknown {
  return parseJson(readTextFixtureByPath(relativePath));
}

export function readTextFixtureByPath(relativePath: string): string {
  const fixtureUrl = new URL(
    `../../../../../tests/contracts/${relativePath}`,
    import.meta.url,
  );

  return readFileSync(fixtureUrl, "utf8");
}

export function readContractManifest(): ContractManifest {
  const parsed = readContractFixture("manifest.json");
  if (!isContractManifest(parsed)) {
    throw new Error("Contract fixture manifest has an unexpected shape");
  }
  if (parsed.manifestVersion !== 1) {
    throw new Error(
      `Unsupported contract manifest version ${parsed.manifestVersion}`,
    );
  }

  return parsed;
}

export function manifestEntriesFor(
  family: string,
  decoder?: string,
): ContractManifestFixture[] {
  const entries = readContractManifest().fixtures.filter((entry) => {
    if (entry.family !== family) return false;
    if (decoder === undefined) {
      return entry.languageDecoders.typescript === entry.kind;
    }
    return entry.languageDecoders.typescript === decoder;
  });
  if (entries.length === 0) {
    throw new Error(
      `No ${family} fixtures for TypeScript decoder ${decoder ?? "<kind>"}`,
    );
  }

  return entries;
}

export function contractsRoot(): string {
  return new URL("../../../../../tests/contracts", import.meta.url).pathname;
}

export function discoverFixturePaths(root: string, dir = root): string[] {
  return readdirSync(dir).flatMap((entryName) => {
    const absolutePath = join(dir, entryName);
    if (statSync(absolutePath).isDirectory()) {
      return discoverFixturePaths(root, absolutePath);
    }
    if (!absolutePath.endsWith(".json") && !absolutePath.endsWith(".ndjson")) {
      return [];
    }

    const relativePath = relative(root, absolutePath).replaceAll("\\", "/");
    // manifest.json indexes the decoder fixtures; timestamps.json holds shared
    // RFC 3339 policy vectors for the timestamp-guard parity tests, not a
    // per-language decoder fixture; generated/ is emitted separately.
    return relativePath === "manifest.json" ||
      relativePath === "timestamps.json" ||
      relativePath.startsWith("generated/")
      ? []
      : [relativePath];
  });
}

export function parseJson(text: string): unknown {
  const parsed: unknown = JSON.parse(text);

  return parsed;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isContractManifest(value: unknown): value is ContractManifest {
  return (
    isRecord(value) &&
    value.manifestVersion === 1 &&
    Array.isArray(value.fixtures) &&
    value.fixtures.every(isContractManifestFixture)
  );
}

function isContractManifestFixture(
  value: unknown,
): value is ContractManifestFixture {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.family === "string" &&
    typeof value.path === "string" &&
    typeof value.format === "string" &&
    typeof value.kind === "string" &&
    isRecord(value.languageDecoders) &&
    (value.languageDecoders.typescript === undefined ||
      typeof value.languageDecoders.typescript === "string")
  );
}
