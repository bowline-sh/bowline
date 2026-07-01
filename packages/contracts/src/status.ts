import type {
  DeviceId,
  EnvRecordId,
  EventId,
  LeaseId,
  PolicyVersion,
  ProjectId,
  SnapshotId,
} from "./ids";
import type { AccessFlag, MaterializationMode, PathClassification } from "./policy";
import type { EventName } from "./event-names";

export const STATUS_LEVELS = ["healthy", "attention", "limited"] as const;
export type StatusLevel = (typeof STATUS_LEVELS)[number];

export type WorkspaceStatus = {
  readonly level: StatusLevel;
  readonly attentionItems: readonly string[];
};

export const STATUS_SCOPES = ["project", "workspace", "lease"] as const;
export type StatusScope = (typeof STATUS_SCOPES)[number];

export type StatusItemKind =
  | "continuity"
  | "policy"
  | "device"
  | "conflict"
  | "work-view"
  | "lease"
  | "watcher"
  | "env"
  | "hydration"
  | "source"
  | "setup"
  | "metadata"
  | "materialization"
  | "network"
  | "index";

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
  | "hydration"
  | "lease"
  | "overlay"
  | "device"
  | "device-approval-request"
  | "metadata"
  | "component"
  | "index";

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
  readonly unavailableBecause: string;
  readonly stillWorks: readonly string[];
  readonly path?: string;
};

export type ComponentState = "ready" | "degraded" | "unavailable";
export type NetworkState = "online" | "degraded" | "offline";

export const INDEX_STATES = [
  "ready",
  "stale",
  "rebuilding",
  "degraded",
] as const;
export type IndexState = (typeof INDEX_STATES)[number];

export type IndexDegradedReason =
  | "missing"
  | "corrupt"
  | "unsupported"
  | "policy-limited"
  | "rebuild-failed";

export type IndexStatus = {
  readonly state: IndexState;
  readonly source: "local" | "encrypted-index-pack" | "none";
  readonly indexedAt?: string;
  readonly updatedAt?: string;
  readonly snapshotId?: SnapshotId;
  readonly indexPackObjectKey?: string;
  readonly pathCount: number;
  readonly fileCount: number;
  readonly indexedBytes: number;
  readonly pendingPathCount?: number;
  readonly degradedReason?: IndexDegradedReason;
  readonly summary: string;
  readonly nextAction?: SafeAction;
};

export const HYDRATION_BUDGET_STATES = [
  "available",
  "exhausted",
  "unavailable",
] as const;
export type HydrationBudgetState = (typeof HYDRATION_BUDGET_STATES)[number];

export type HydrationBudgetStatus = {
  readonly state: HydrationBudgetState;
  readonly limitBytes: number;
  readonly usedBytes: number;
  readonly reservedBytes: number;
  readonly remainingBytes: number;
  readonly scope: "lease" | "project" | "workspace";
  readonly leaseId?: LeaseId;
  readonly projectId?: ProjectId;
  readonly resetAt?: string;
  readonly nextAction?: SafeAction;
};

export type HydrationProgress = {
  readonly projectId?: ProjectId;
  readonly bytesDone: number;
  readonly bytesRemaining: number;
  readonly cause: string;
};

export type SyncQueueStatus = {
  readonly queued: number;
  readonly claimed: number;
  readonly waitingRetry: number;
  readonly blockedOffline: number;
  readonly attention: number;
  readonly completed: number;
};

export type EventWatermarks = {
  readonly lastScanAt?: string;
  readonly lastEventId?: EventId;
  readonly eventLagMs?: number;
  readonly syncState?: ComponentState;
  readonly watcherState?: ComponentState;
  readonly networkState?: NetworkState;
};

export type SafeAction = {
  readonly label: string;
  readonly command?: string;
  readonly effectCategory?: SafeActionEffect;
  readonly targetKind?: SafeActionTarget;
};

export type SafeActionEffect =
  | "inspect"
  | "trust"
  | "setup"
  | "mutate"
  | "destructive";

export type SafeActionTarget =
  | "workspace"
  | "device"
  | "setup"
  | "work-view"
  | "conflict"
  | "agent"
  | "recovery"
  | "unknown";

export type WorkspaceSummary = {
  readonly projectsNeedingAttention?: readonly ProjectAttentionSummary[];
  readonly totalProjects?: number;
  readonly observed?: ObservedWorkspaceSummary;
};

export type ObservedWorkspaceSummary = {
  readonly repoCount: number;
  readonly noRemoteRepoCount: number;
  readonly staleRemoteTrackingRepoCount: number;
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
