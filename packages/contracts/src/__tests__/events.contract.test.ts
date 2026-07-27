import { describe, expect, it } from "vitest";
import { statSync } from "node:fs";

import { isWatchFrame, isWorkspaceEvent } from "../index";
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
