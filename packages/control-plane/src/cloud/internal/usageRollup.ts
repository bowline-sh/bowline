// Canonical pure rollup math for beta usage instrumentation (pricing).
//
// The region between the PARITY markers below is duplicated byte-for-byte in
// `convex/lib/usageRollup.ts` because Convex functions cannot import from
// `src/` (separate TS project, and the no-`../src` architecture boundary). The
// parity test in `usageRollup.contract.test.ts` reads both files and asserts the
// regions are identical, so any edit here MUST be applied to both copies.
//
// This module is intentionally framework-free: plain numbers/strings/booleans,
// no Convex or @bowline/contracts coupling, and no wall-clock access (the caller
// passes `day` and `generatedAt`). That keeps it fully unit-testable and lets the
// Convex copy stay free of bundled dependencies.

// === USAGE ROLLUP PARITY START ===
export type UsageStorageByKind = {
  readonly indexPack: number;
  readonly locatorIndex: number;
  readonly overlayPack: number;
  readonly snapshotManifest: number;
  readonly sourcePack: number;
};

export type UsageRollupInputs = {
  readonly day: string;
  readonly workspaceId: string;
  readonly accountId: string;
  readonly workOsOrganizationId?: string;
  readonly generatedAt: string;
  readonly storageBytesCurrent: number;
  readonly storageBytesRetained: number;
  readonly storageBytesByKind: UsageStorageByKind;
  readonly storageObjectCount: number;
  readonly snapshotCount: number;
  readonly oldestRetainedManifestCreatedAt?: string;
  readonly eventsCumulative: number;
  readonly uploadsCommittedCumulative: number;
  readonly downloadsCumulative: number;
  readonly downloadBytesCumulative: number;
  readonly leasesActiveCount: number;
  readonly leasesCreatedCumulative: number;
  readonly agentOverlayBytes: number;
  readonly authorizedDeviceCount: number;
  readonly deviceCountByPlatform: Readonly<Record<string, number>>;
  readonly totalProjects: number;
  readonly repoCount: number;
  readonly envFileCount: number;
  readonly fileCount: number;
  readonly pathCount: number;
  readonly conflictsOpen: number;
  readonly conflictsDetectedCumulative: number;
  readonly conflictsResolvedCumulative: number;
  readonly prior?: UsageDailyRollupRow;
};

export type UsageDailyRollupRow = {
  readonly accountId: string;
  readonly activeDay: boolean;
  readonly agentOverlayBytes: number;
  readonly authorizedDeviceCount: number;
  readonly conflictsDetectedCumulative: number;
  readonly conflictsDetectedDelta: number;
  readonly conflictsOpen: number;
  readonly conflictsResolvedCumulative: number;
  readonly conflictsResolvedDelta: number;
  readonly day: string;
  readonly deviceCountByPlatform: Readonly<Record<string, number>>;
  readonly downloadBytesCumulative: number;
  readonly downloadBytesDelta: number;
  readonly downloadsCumulative: number;
  readonly downloadsDelta: number;
  readonly envFileCount: number;
  readonly eventsCumulative: number;
  readonly eventsDelta: number;
  readonly fileCount: number;
  readonly generatedAt: string;
  readonly leasesActiveCount: number;
  readonly leasesCreatedCumulative: number;
  readonly leasesCreatedDelta: number;
  readonly oldestRetainedAgeDays: number;
  readonly pathCount: number;
  readonly repoCount: number;
  readonly snapshotCount: number;
  readonly storageBytesByKind: UsageStorageByKind;
  readonly storageBytesCurrent: number;
  readonly storageBytesRetained: number;
  readonly storageObjectCount: number;
  readonly totalProjects: number;
  readonly uploadsCommittedCumulative: number;
  readonly uploadsCommittedDelta: number;
  readonly workOsOrganizationId?: string;
  readonly workspaceId: string;
};

const MS_PER_DAY = 86_400_000;

export function utcDayString(iso: string): string {
  return iso.slice(0, 10);
}

function nonNegativeDelta(current: number, prior: number | undefined): number {
  return Math.max(0, current - (prior ?? 0));
}

function computeOldestRetainedAgeDays(
  day: string,
  oldestRetainedManifestCreatedAt: string | undefined,
): number {
  if (oldestRetainedManifestCreatedAt === undefined) return 0;
  const dayMs = Date.parse(`${day}T00:00:00.000Z`);
  const createdMs = Date.parse(oldestRetainedManifestCreatedAt);
  if (Number.isNaN(dayMs) || Number.isNaN(createdMs)) return 0;
  return Math.max(0, Math.floor((dayMs - createdMs) / MS_PER_DAY));
}

export function computeUsageDailyRollup(
  inputs: UsageRollupInputs,
): UsageDailyRollupRow {
  const prior = inputs.prior;
  const eventsDelta = nonNegativeDelta(
    inputs.eventsCumulative,
    prior?.eventsCumulative,
  );
  const uploadsCommittedDelta = nonNegativeDelta(
    inputs.uploadsCommittedCumulative,
    prior?.uploadsCommittedCumulative,
  );
  const downloadsDelta = nonNegativeDelta(
    inputs.downloadsCumulative,
    prior?.downloadsCumulative,
  );
  const downloadBytesDelta = nonNegativeDelta(
    inputs.downloadBytesCumulative,
    prior?.downloadBytesCumulative,
  );
  const leasesCreatedDelta = nonNegativeDelta(
    inputs.leasesCreatedCumulative,
    prior?.leasesCreatedCumulative,
  );
  const conflictsDetectedDelta = nonNegativeDelta(
    inputs.conflictsDetectedCumulative,
    prior?.conflictsDetectedCumulative,
  );
  const conflictsResolvedDelta = nonNegativeDelta(
    inputs.conflictsResolvedCumulative,
    prior?.conflictsResolvedCumulative,
  );
  const activeDay =
    eventsDelta > 0 ||
    uploadsCommittedDelta > 0 ||
    downloadsDelta > 0 ||
    leasesCreatedDelta > 0 ||
    conflictsDetectedDelta > 0 ||
    conflictsResolvedDelta > 0;

  return {
    accountId: inputs.accountId,
    activeDay,
    agentOverlayBytes: inputs.agentOverlayBytes,
    authorizedDeviceCount: inputs.authorizedDeviceCount,
    conflictsDetectedCumulative: inputs.conflictsDetectedCumulative,
    conflictsDetectedDelta,
    conflictsOpen: inputs.conflictsOpen,
    conflictsResolvedCumulative: inputs.conflictsResolvedCumulative,
    conflictsResolvedDelta,
    day: inputs.day,
    deviceCountByPlatform: inputs.deviceCountByPlatform,
    downloadBytesCumulative: inputs.downloadBytesCumulative,
    downloadBytesDelta,
    downloadsCumulative: inputs.downloadsCumulative,
    downloadsDelta,
    envFileCount: inputs.envFileCount,
    eventsCumulative: inputs.eventsCumulative,
    eventsDelta,
    fileCount: inputs.fileCount,
    generatedAt: inputs.generatedAt,
    leasesActiveCount: inputs.leasesActiveCount,
    leasesCreatedCumulative: inputs.leasesCreatedCumulative,
    leasesCreatedDelta,
    oldestRetainedAgeDays: computeOldestRetainedAgeDays(
      inputs.day,
      inputs.oldestRetainedManifestCreatedAt,
    ),
    pathCount: inputs.pathCount,
    repoCount: inputs.repoCount,
    snapshotCount: inputs.snapshotCount,
    storageBytesByKind: inputs.storageBytesByKind,
    storageBytesCurrent: inputs.storageBytesCurrent,
    storageBytesRetained: inputs.storageBytesRetained,
    storageObjectCount: inputs.storageObjectCount,
    totalProjects: inputs.totalProjects,
    uploadsCommittedCumulative: inputs.uploadsCommittedCumulative,
    uploadsCommittedDelta,
    ...(inputs.workOsOrganizationId === undefined
      ? {}
      : { workOsOrganizationId: inputs.workOsOrganizationId }),
    workspaceId: inputs.workspaceId,
  };
}
// === USAGE ROLLUP PARITY END ===
