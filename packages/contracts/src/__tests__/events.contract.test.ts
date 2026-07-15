import { describe, expect, it } from "vitest";
import { statSync } from "node:fs";

import {
  isContentLayout,
  isSnapshotManifest,
  isWatchFrame,
  isWorkspaceEvent,
  SNAPSHOT_SCHEMA_VERSION,
} from "../index";
import {
  contractsRoot,
  discoverFixturePaths,
  isRecord,
  manifestEntriesFor,
  parseJson,
  readContractFixture,
  readContractManifest,
  readTextFixtureByPath,
} from "./support/contractFixtures";

describe("workspace event contract", () => {
  it("keeps the manifest in sync with every JSON and NDJSON fixture", () => {
    const listed = readContractManifest()
      .fixtures.map((entry) => {
        expect(["json", "ndjson"]).toContain(entry.format);
        expect(
          statSync(
            new URL(
              `../../../../tests/contracts/${entry.path}`,
              import.meta.url,
            ),
          ).isFile(),
        ).toBe(true);
        return entry.path;
      })
      .sort();
    const discovered = discoverFixturePaths(contractsRoot()).sort();

    expect(listed).toEqual(discovered);
  });

  it("accepts every workspace event fixture listed in the manifest", () => {
    for (const entry of manifestEntriesFor("events", "WorkspaceEvent")) {
      expect(entry.format).toBe("json");
      expect(isWorkspaceEvent(readJsonFixtureByPath(entry.path))).toBe(true);
    }
  });

  it("accepts every watch stream fixture listed in the manifest", () => {
    for (const entry of manifestEntriesFor("streams", "WatchFrame")) {
      expect(entry.format).toBe("ndjson");
      const frames = readTextFixtureByPath(entry.path)
        .trim()
        .split("\n")
        .map(parseJson);

      expect(frames.length).toBeGreaterThan(0);
      expect(frames.every(isWatchFrame)).toBe(true);
    }
  });

  it("accepts every snapshot fixture listed in the manifest", () => {
    for (const entry of manifestEntriesFor("snapshots", "SnapshotManifest")) {
      expect(entry.format).toBe("json");
      expect(isSnapshotManifest(readJsonFixtureByPath(entry.path))).toBe(true);
    }
  });

  it("accepts every versioned content-layout fixture", () => {
    for (const entry of manifestEntriesFor("snapshots", "ContentLayout")) {
      expect(entry.format).toBe("json");
      expect(isContentLayout(readJsonFixtureByPath(entry.path))).toBe(true);
    }
  });

  it("rejects malformed segmented content layouts", () => {
    const layout = readJsonFixture("snapshots/content-layout-segmented-v1");
    expect(isRecord(layout)).toBe(true);
    if (!isRecord(layout) || !Array.isArray(layout.segments)) return;
    const segments: unknown[] = layout.segments;

    expect(isContentLayout({ ...layout, logicalLength: 11 })).toBe(false);
    expect(isContentLayout({ ...layout, logicalContentId: "" })).toBe(false);
    expect(
      isContentLayout({
        ...layout,
        segments: segments.map((segment, index) =>
          index === 1 && isRecord(segment)
            ? { ...segment, ordinal: 2 }
            : segment,
        ),
      }),
    ).toBe(false);
    expect(isContentLayout({ ...layout, segmentSize: 0 })).toBe(false);
    expect(isContentLayout({ ...layout, kind: "packed-record-v1" })).toBe(
      false,
    );
  });

  it("accepts the shared metadata-corrupt event fixture", () => {
    expect(isWorkspaceEvent(readJsonFixture("events/metadata-corrupt"))).toBe(
      true,
    );
  });

  it("accepts newline-delimited status watch frames", () => {
    const frames = readTextFixture("streams/status-watch.ndjson")
      .trim()
      .split("\n")
      .map(parseJson);

    expect(frames.length).toBeGreaterThan(0);
    expect(frames.every(isWatchFrame)).toBe(true);
  });

  it("requires watch sequence numbers to be nonnegative integers", () => {
    const frame = readTextFixture("streams/status-watch.ndjson")
      .trim()
      .split("\n")
      .map(parseJson)[0];
    expect(isRecord(frame)).toBe(true);
    if (!isRecord(frame)) return;

    expect(isWatchFrame({ ...frame, sequence: -1 })).toBe(false);
    expect(isWatchFrame({ ...frame, sequence: 1.5 })).toBe(false);
  });

  it("applies status numeric refinements inside the selected watch union", () => {
    const frame = readTextFixture("streams/status-watch.ndjson")
      .trim()
      .split("\n")
      .map(parseJson)[0];
    expect(isRecord(frame)).toBe(true);
    if (!isRecord(frame)) return;
    const status = isRecord(frame.status) ? frame.status : {};
    const eventWatermarks = isRecord(status.eventWatermarks)
      ? status.eventWatermarks
      : {};

    expect(
      isWatchFrame({
        ...frame,
        status: {
          ...status,
          eventWatermarks: { ...eventWatermarks, eventLagMs: -1 },
        },
      }),
    ).toBe(false);
  });

  it("accepts the shared mixed-tree snapshot manifest fixture", () => {
    const fixture = readJsonFixture("snapshots/mixed-tree");

    expect(isSnapshotManifest(fixture)).toBe(true);
    if (!isSnapshotManifest(fixture)) return;

    expect(fixture.schemaVersion).toBe(SNAPSHOT_SCHEMA_VERSION);
    expect(fixture.namespaceRootId).toMatch(/^nsp_[a-f0-9]{64}$/u);
    expect(fixture.semanticManifestDigest).toMatch(/^[a-f0-9]{64}$/u);
    expect(fixture.entryCount).toBe(7);
    expect(
      isSnapshotManifest({
        ...fixture,
        entries: [],
      }),
    ).toBe(false);
  });

  it("rejects snapshot manifests outside the current page-root grammar", () => {
    const fixture = readJsonFixture("snapshots/mixed-tree");
    expect(
      isSnapshotManifest({ ...asRecord(fixture), namespaceRootId: "nsp_bad" }),
    ).toBe(false);
    expect(
      isSnapshotManifest({
        ...asRecord(fixture),
        semanticManifestDigest: "not-a-digest",
      }),
    ).toBe(false);
    expect(isSnapshotManifest({ ...asRecord(fixture), entryCount: -1 })).toBe(
      false,
    );
  });
});

function readJsonFixture(name: string): unknown {
  return readContractFixture(`${name}.json`);
}

function readJsonFixtureByPath(relativePath: string): unknown {
  return readContractFixture(relativePath);
}

function readTextFixture(name: string): string {
  return readTextFixtureByPath(name);
}

function asRecord(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) throw new Error("fixture should be an object");
  return value;
}
