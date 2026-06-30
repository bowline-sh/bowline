import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

import { isSnapshotManifest, isWatchFrame, isWorkspaceEvent } from "../index";

describe("workspace event contract", () => {
  it("accepts the shared metadata-corrupt event fixture", () => {
    expect(isWorkspaceEvent(readJsonFixture("events/metadata-corrupt"))).toBe(
      true,
    );
  });

  it("accepts newline-delimited status watch frames", () => {
    const frames = readTextFixture("streams/status-watch.ndjson")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line) as unknown);

    expect(frames.length).toBeGreaterThan(0);
    expect(frames.every(isWatchFrame)).toBe(true);
  });

  it("accepts the shared mixed-tree snapshot manifest fixture", () => {
    expect(isSnapshotManifest(readJsonFixture("snapshots/mixed-tree"))).toBe(
      true,
    );
  });
});

function readJsonFixture(name: string): unknown {
  return JSON.parse(readTextFixture(`${name}.json`)) as unknown;
}

function readTextFixture(name: string): string {
  const fixtureUrl = new URL(
    `../../../../tests/contracts/${name}`,
    import.meta.url,
  );

  return readFileSync(fixtureUrl, "utf8");
}
