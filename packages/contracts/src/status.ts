import type {
  DeviceId,
  EnvRecordId,
  EventId,
  LeaseId,
  PolicyVersion,
  ProjectId,
  SnapshotId,
} from "./ids";
import type {
  AccessFlag,
  MaterializationMode,
  PathClassification,
} from "./policy";
import type { EventName } from "./event-names";

export const STATUS_LEVELS = ["healthy", "attention", "limited"] as const;
export type StatusLevel = (typeof STATUS_LEVELS)[number];

export type WorkspaceStatus = {
  readonly level: StatusLevel;
  readonly attentionItems: readonly string[];
};

export const STATUS_SCOPES = ["project", "workspace", "lease"] as const;
export type StatusScope = (typeof STATUS_SCOPES)[number];

export const FRESHNESS_VERDICTS = [
  "current",
  "behind",
  "diverged",
  "unknown",
] as const;
export type FreshnessVerdict = (typeof FRESHNESS_VERDICTS)[number];

export const FRESHNESS_AXES = ["snapshot", "git"] as const;
export type FreshnessAxis = (typeof FRESHNESS_AXES)[number];

export type StaleBaseStatus = {
  readonly axis: FreshnessAxis;
  readonly verdict: FreshnessVerdict;
  readonly summary: string;
  readonly remedyCommand?: string;
  readonly projectId?: ProjectId;
  readonly projectPath?: string;
  readonly baseSnapshotId?: SnapshotId;
  readonly latestSnapshotId?: SnapshotId;
};

export type StatusItemKind =
  | "continuity"
  | "policy"
  | "device"
  | "conflict"
  | "work-view"
  | "lease"
  | "watcher"
  | "env"
  | "source"
  | "setup"
  | "metadata"
  | "materialization"
  | "network"
  | "update";

export type StatusSubjectKind =
  | "workspace"
  | "root"
  | "project"
  | "path"
  | "snapshot"
  | "env-record"
  | "policy"
  | "setup-receipt"
  | "conflict"
  | "work-view"
  | "lease"
  | "overlay"
  | "device"
  | "device-approval-request"
  | "metadata"
  | "component";

export type StatusSubject = {
  readonly kind: StatusSubjectKind;
  readonly id: string;
  readonly path?: string;
};

export type StatusItem = {
  readonly kind: StatusItemKind;
  readonly summary: string;
  readonly subject?: StatusSubject;
  readonly path?: string;
  readonly classification?: PathClassification;
  readonly mode?: MaterializationMode;
  readonly access?: readonly AccessFlag[];
  readonly eventId?: EventId;
  readonly eventName?: EventName;
  readonly deviceId?: DeviceId;
  readonly leaseId?: LeaseId;
  readonly projectId?: ProjectId;
  readonly snapshotId?: SnapshotId;
  readonly policyVersion?: PolicyVersion;
  readonly envRecordId?: EnvRecordId;
};

export type LimitedCapability = {
  readonly capability: string;
  readonly supportCapability?: ControlPlaneSupportCapability;
  readonly unavailableBecause: string;
  readonly stillWorks: readonly string[];
  readonly path?: string;
};

export const CONTROL_PLANE_SUPPORT_CAPABILITIES = [
  "device-approval",
  "project-scoped-workspace-ref-cas",
  "work-view",
  "agent-lease",
  "encrypted-object-store",
  "recovery",
] as const;
export type ControlPlaneSupportCapability =
  (typeof CONTROL_PLANE_SUPPORT_CAPABILITIES)[number];

export const PROJECT_SETUP_READINESS_STATES = [
  "unknown",
  "runnable",
  "needs-setup",
  "blocked",
] as const;
export type ProjectSetupReadinessState =
  (typeof PROJECT_SETUP_READINESS_STATES)[number];

export const PROJECT_SETUP_RECEIPT_STATES = [
  "approved",
  "approval-required",
  "completed",
  "failed",
] as const;
export type ProjectSetupReceiptState =
  (typeof PROJECT_SETUP_RECEIPT_STATES)[number];

export type ProjectSetupReadiness = {
  readonly state: ProjectSetupReadinessState;
  readonly reason: string;
  readonly remedy?: string;
  readonly identityHash?: string;
  readonly latestReceiptId?: string;
  readonly latestReceiptState?: ProjectSetupReceiptState;
  readonly updatedAt?: string;
};

export type SyncQueueStatus = {
  readonly queued: number;
  readonly claimed: number;
  readonly waitingRetry: number;
  readonly blockedOffline: number;
  readonly reconciliationRequired: number;
  readonly attention: number;
  readonly completed: number;
};

export type EventWatermarks = {
  readonly lastScanAt?: string;
  readonly lastEventId?: EventId;
  readonly eventLagMs?: number;
};

/**
 * A concrete, runnable command that repairs the current workspace/account
 * state. Producers set `label`, `command`, and `mutates` directly; `mutates` is
 * never inferred from the command string. `mutates` drives the TUI's
 * confirm-before-run gate.
 */
export type RepairCommand = {
  readonly label: string;
  readonly command?: string;
  readonly mutates: boolean;
};

/**
 * A pending device-approval affordance. `code` and `approveCommand` are
 * sensitive local trust material carried only on trusted local status surfaces
 * (CLI/TUI/menu bar); they must never appear in hosted dashboard payloads,
 * persisted snapshots, shared fixtures, analytics, or logs.
 */
export type { DeviceApprovalAffordance } from "./generated/wire-contracts";

export type WorkspaceSummary = {
  readonly projectsNeedingAttention?: readonly ProjectAttentionSummary[];
  readonly totalProjects?: number;
  readonly observed?: ObservedWorkspaceSummary;
};

export type ObservedWorkspaceSummary = {
  readonly repoCount: number;
  readonly noRemoteRepoCount: number;
  readonly staleRemoteTrackingRepoCount: number;
  readonly gitPartialProjectCount: number;
  readonly gitUnavailableProjectCount: number;
  readonly generatedPathCount: number;
  readonly dependencyPathCount: number;
  readonly envFileCount: number;
  readonly untrackedFileCount: number;
  readonly localOnlyPathCount: number;
  readonly blockedPathCount: number;
  readonly workspaceSyncPathCount: number;
};

export type ProjectAttentionSummary = {
  readonly projectId: ProjectId;
  readonly path: string;
  readonly level: StatusLevel;
  readonly summary: string;
};
