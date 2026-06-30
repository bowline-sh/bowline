import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import {
  computeUsageDailyRollup,
  utcDayString,
  type UsageRollupInputs,
} from "../index";

function makeInputs(
  overrides: Partial<UsageRollupInputs> = {},
): UsageRollupInputs {
  return {
    accountId: "account_user",
    agentOverlayBytes: 0,
    authorizedDeviceCount: 0,
    conflictsDetectedCumulative: 0,
    conflictsOpen: 0,
    conflictsResolvedCumulative: 0,
    day: "2026-06-30",
    deviceCountByPlatform: {},
    downloadBytesCumulative: 0,
    downloadsCumulative: 0,
    envFileCount: 0,
    eventsCumulative: 0,
    fileCount: 0,
    generatedAt: "2026-06-30T08:00:00.000Z",
    leasesActiveCount: 0,
    leasesCreatedCumulative: 0,
    pathCount: 0,
    repoCount: 0,
    snapshotCount: 0,
    storageBytesByKind: {
      indexPack: 0,
      locatorIndex: 0,
      overlayPack: 0,
      snapshotManifest: 0,
      sourcePack: 0,
    },
    storageBytesCurrent: 0,
    storageBytesRetained: 0,
    storageObjectCount: 0,
    totalProjects: 0,
    uploadsCommittedCumulative: 0,
    workspaceId: "workspace_code",
    ...overrides,
  };
}

const PARITY_START = "// === USAGE ROLLUP PARITY START ===";
const PARITY_END = "// === USAGE ROLLUP PARITY END ===";

function parityRegion(relativePath: string): string {
  const source = readFileSync(join(process.cwd(), relativePath), "utf8");
  const start = source.indexOf(PARITY_START);
  const end = source.indexOf(PARITY_END);
  expect(start, `${relativePath} missing parity start marker`).toBeGreaterThan(
    -1,
  );
  expect(end, `${relativePath} missing parity end marker`).toBeGreaterThan(
    start,
  );
  return source.slice(start, end + PARITY_END.length).trim();
}

function readConvexSource(relativePath: string): string {
  return readFileSync(join(process.cwd(), relativePath), "utf8");
}

describe("computeUsageDailyRollup", () => {
  it("treats first-day deltas (no prior) as the full cumulative", () => {
    const row = computeUsageDailyRollup(
      makeInputs({
        conflictsDetectedCumulative: 2,
        conflictsResolvedCumulative: 1,
        downloadBytesCumulative: 4096,
        downloadsCumulative: 5,
        eventsCumulative: 42,
        leasesCreatedCumulative: 3,
        uploadsCommittedCumulative: 9,
      }),
    );

    expect(row.eventsDelta).toBe(42);
    expect(row.uploadsCommittedDelta).toBe(9);
    expect(row.downloadsDelta).toBe(5);
    expect(row.downloadBytesDelta).toBe(4096);
    expect(row.leasesCreatedDelta).toBe(3);
    expect(row.conflictsDetectedDelta).toBe(2);
    expect(row.conflictsResolvedDelta).toBe(1);
    expect(row.activeDay).toBe(true);
  });

  it("derives second-day deltas as the difference from the prior rollup", () => {
    const prior = computeUsageDailyRollup(
      makeInputs({
        day: "2026-06-29",
        downloadBytesCumulative: 1000,
        downloadsCumulative: 5,
        eventsCumulative: 42,
        uploadsCommittedCumulative: 9,
      }),
    );
    const row = computeUsageDailyRollup(
      makeInputs({
        downloadBytesCumulative: 1500,
        downloadsCumulative: 8,
        eventsCumulative: 50,
        prior,
        uploadsCommittedCumulative: 11,
      }),
    );

    expect(row.eventsDelta).toBe(8);
    expect(row.uploadsCommittedDelta).toBe(2);
    expect(row.downloadsDelta).toBe(3);
    expect(row.downloadBytesDelta).toBe(500);
    expect(row.eventsCumulative).toBe(50);
  });

  it("clamps deltas to zero when a cumulative counter regresses", () => {
    const prior = computeUsageDailyRollup(
      makeInputs({ day: "2026-06-29", eventsCumulative: 100 }),
    );
    const row = computeUsageDailyRollup(
      makeInputs({ eventsCumulative: 40, prior }),
    );

    expect(row.eventsDelta).toBe(0);
    expect(row.eventsCumulative).toBe(40);
  });

  it("marks a day inactive when no volume metric advanced", () => {
    const prior = computeUsageDailyRollup(
      makeInputs({ day: "2026-06-29", eventsCumulative: 42 }),
    );
    const row = computeUsageDailyRollup(
      makeInputs({ eventsCumulative: 42, prior }),
    );

    expect(row.activeDay).toBe(false);
    expect(row.eventsDelta).toBe(0);
  });

  it("passes storage current/retained/byKind through unchanged", () => {
    const row = computeUsageDailyRollup(
      makeInputs({
        storageBytesByKind: {
          indexPack: 30,
          locatorIndex: 40,
          overlayPack: 20,
          snapshotManifest: 50,
          sourcePack: 10,
        },
        storageBytesCurrent: 100,
        storageBytesRetained: 50,
        storageObjectCount: 7,
      }),
    );

    expect(row.storageBytesCurrent).toBe(100);
    expect(row.storageBytesRetained).toBe(50);
    expect(row.storageObjectCount).toBe(7);
    expect(row.storageBytesByKind.sourcePack).toBe(10);
    expect(row.storageBytesByKind.snapshotManifest).toBe(50);
  });

  it("computes oldest-retained age in whole UTC days (partial days floor)", () => {
    // 2026-06-20T12:00Z → 2026-06-30T00:00Z is 9.5 days, floored to 9.
    expect(
      computeUsageDailyRollup(
        makeInputs({
          oldestRetainedManifestCreatedAt: "2026-06-20T12:00:00.000Z",
        }),
      ).oldestRetainedAgeDays,
    ).toBe(9);

    expect(computeUsageDailyRollup(makeInputs()).oldestRetainedAgeDays).toBe(0);

    expect(
      computeUsageDailyRollup(
        makeInputs({ oldestRetainedManifestCreatedAt: "not-a-date" }),
      ).oldestRetainedAgeDays,
    ).toBe(0);
  });

  it("omits the optional org id when absent and keeps it when present", () => {
    expect(
      "workOsOrganizationId" in computeUsageDailyRollup(makeInputs()),
    ).toBe(false);
    expect(
      computeUsageDailyRollup(makeInputs({ workOsOrganizationId: "org_acme" }))
        .workOsOrganizationId,
    ).toBe("org_acme");
  });

  it("buckets an ISO timestamp into its UTC day", () => {
    expect(utcDayString("2026-06-30T08:00:00.000Z")).toBe("2026-06-30");
  });
});

describe("usage rollup contract parity", () => {
  it("keeps the canonical and Convex rollup math byte-identical", () => {
    const canonical = parityRegion("src/cloud/internal/usageRollup.ts");
    const convexCopy = parityRegion("convex/lib/usageRollup.ts");
    expect(canonical.length).toBeGreaterThan(500);
    expect(convexCopy).toBe(canonical);
  });

  it("defines both rollup tables and their indexes in the Convex schema", () => {
    const schema = readConvexSource("convex/schema.ts");
    expect(schema).toContain("downloadCounters: defineTable");
    expect(schema).toContain("usageDailyRollups: defineTable");
    expect(schema).toContain('.index("by_account_day", ["accountId", "day"])');
    expect(schema).toContain(
      '.index("by_workspace_day", ["workspaceId", "day"])',
    );
    expect(schema).toContain('.index("by_day", ["day"])');
  });

  it("wires the daily rollup cron to the enumerator", () => {
    const crons = readConvexSource("convex/crons.ts");
    expect(crons).toContain("cronJobs()");
    expect(crons).toContain("crons.daily(");
    expect(crons).toContain("internal.usageRollups.runDailyUsageRollup");
    expect(crons).toContain("export default crons");
  });

  it("fans out the rollup and gates the export query", () => {
    const rollups = readConvexSource("convex/usageRollups.ts");
    expect(rollups).toContain(".paginate(");
    expect(rollups).toContain("ctx.scheduler.runAfter");
    expect(rollups).toContain("by_workspace_retention");
    expect(rollups).toContain("by_workspace_day");
    expect(rollups).toContain("assertControlPlaneAuth");
    expect(rollups).toContain('from "./lib/usageRollup"');
    expect(rollups).not.toContain("../src");
  });

  it("increments the download counter inside the download intent mutation", () => {
    const mutations = readConvexSource("convex/objectMutations.ts");
    expect(mutations).toContain('ctx.db.insert("downloadCounters"');
    expect(mutations).toContain("downloadCount: existingCounter.downloadCount");
    expect(mutations).toContain(
      "Math.max(0, Math.min(args.length, object.byteLength - args.offset))",
    );
  });
});
