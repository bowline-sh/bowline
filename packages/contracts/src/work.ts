import type {
  DeviceId,
  ProjectId,
  SnapshotId,
  WorkspaceId,
  WorkViewId,
} from "./ids";
import type { RepairCommand, WorkspaceStatus } from "./status";
import type { CommandOutputBase } from "./commands";

export const WORK_VIEW_LIFECYCLES = [
  "active",
  "review-ready",
  "accepted",
  "discarded",
  "expired",
  "archived",
] as const;
export type WorkViewLifecycle = (typeof WORK_VIEW_LIFECYCLES)[number];

export const WORK_VIEW_VISIBILITIES = [
  "default-visible",
  "hidden",
  "pinned",
  "followed",
] as const;
export type WorkViewVisibility = (typeof WORK_VIEW_VISIBILITIES)[number];

export const WORK_VIEW_SYNC_STATES = [
  "local-only",
  "synced",
  "uploading",
  "attention",
  "conflicted",
] as const;
export type WorkViewSyncState = (typeof WORK_VIEW_SYNC_STATES)[number];

export const WORK_VIEW_RETENTION_STATES = [
  "current",
  "retained",
  "expired",
  "delete-eligible",
] as const;
export type WorkViewRetentionState =
  (typeof WORK_VIEW_RETENTION_STATES)[number];

export type WorkViewRetention = {
  readonly state: WorkViewRetentionState;
  readonly retainUntil?: string;
  readonly restorable: boolean;
};

export type WorkView = {
  readonly id: WorkViewId;
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly projectPath: string;
  readonly name: string;
  readonly visiblePath: string;
  readonly baseSnapshotId: SnapshotId;
  readonly overlayHead: string;
  readonly overlayVersion: number;
  readonly envProfile: string;
  readonly lifecycle: WorkViewLifecycle;
  readonly visibility: WorkViewVisibility;
  readonly syncState: WorkViewSyncState;
  readonly retention: WorkViewRetention;
  readonly ownerDeviceId?: DeviceId;
  readonly followedBy: readonly string[];
  readonly hostMaterializations: readonly string[];
  readonly attention: readonly string[];
  readonly createdAt: string;
  readonly updatedAt: string;
};

export const WORK_DIFF_CHANGE_KINDS = [
  "added",
  "modified",
  "deleted",
  "policy-review",
  "conflict",
] as const;
export type WorkDiffChangeKind = (typeof WORK_DIFF_CHANGE_KINDS)[number];

export type WorkDiffEntry = {
  readonly path: string;
  readonly kind: WorkDiffChangeKind;
  readonly summary: string;
  readonly containsSecrets: boolean;
};

export const WORK_COMMAND_ACTIONS = [
  "created",
  "reused",
  "listed",
  "diffed",
  "review-ready",
  "accepted",
  "discarded",
  "restored",
  "cleanup-previewed",
  "cleanup-applied",
] as const;
export type WorkCommandAction = (typeof WORK_COMMAND_ACTIONS)[number];

export type WorkCreateCommandOutput = CommandOutputBase<"work create"> & {
  readonly action: "created" | "reused";
  readonly workView: WorkView;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export type WorkListCommandOutput = CommandOutputBase<"work list"> & {
  readonly action: "listed";
  readonly workspaceId: WorkspaceId;
  readonly workViews: readonly WorkView[];
  readonly includeHidden: boolean;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export type WorkDiffCommandOutput = CommandOutputBase<
  "work review" | "work diff"
> & {
  readonly action: "diffed";
  readonly workView: WorkView;
  readonly changes: readonly WorkDiffEntry[];
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export type WorkLifecycleCommandOutput = CommandOutputBase<
  "work accept" | "work discard" | "work restore"
> & {
  readonly action: "accepted" | "review-ready" | "discarded" | "restored";
  readonly paths?: readonly string[];
  readonly partial?: boolean;
  readonly workView: WorkView;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export type WorkCleanupCommandOutput = CommandOutputBase<"work cleanup"> & {
  readonly action: "cleanup-previewed" | "cleanup-applied";
  readonly workspaceId: WorkspaceId;
  readonly previewedPaths: readonly string[];
  readonly deletedPaths: readonly string[];
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};
