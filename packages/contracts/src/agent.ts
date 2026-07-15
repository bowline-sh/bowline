import type { CommandOutputBase } from "./commands";
import type {
  DeviceId,
  EventId,
  LeaseId,
  PolicyVersion,
  ProjectId,
  SnapshotId,
  WorkspaceId,
  WorkViewId,
} from "./ids";
import type { PathClassification } from "./policy";
import type {
  FreshnessVerdict,
  RepairCommand,
  StaleBaseStatus,
  StatusItem,
  WorkspaceStatus,
} from "./status";

export const AGENT_SESSION_STATES = [
  "provisional",
  "open",
  "completed",
] as const;
export type AgentLeaseSessionState = (typeof AGENT_SESSION_STATES)[number];

export const AGENT_LEASE_DISPATCH_STATES = [
  "none",
  "pending",
  "claimed",
] as const;
export type AgentLeaseDispatchState =
  (typeof AGENT_LEASE_DISPATCH_STATES)[number];

export type AgentLeaseBase = "latest-workspace" | "latest:main";

export type AgentLeaseScope = {
  readonly roots: readonly string[];
  readonly classifications?: readonly PathClassification[];
  readonly maxBytesPerRead?: number;
  readonly maxFilesPerRequest?: number;
  readonly maxDepth?: number;
};

export type AgentLeaseScopes = {
  readonly read: AgentLeaseScope;
  readonly write: AgentLeaseScope;
};

export type AgentEnvRestriction = {
  readonly kind: "allowlist" | "blocked-secret" | "grant-required";
  readonly key: string;
  readonly reason?: string;
  readonly grantId?: string;
};

export type AgentEnvProfile = {
  readonly name: string;
  readonly materialization: "lease-work-view" | "project-path" | "unavailable";
  readonly availableKeys: readonly string[];
  readonly restrictions: readonly AgentEnvRestriction[];
  readonly grantIds: readonly string[];
};

export type AgentOutputTarget =
  | {
      readonly kind: "real-project";
      readonly path: string;
    }
  | {
      readonly kind: "work-view";
      readonly workViewId: WorkViewId;
      readonly path: string;
    };

export type AgentAuditPointer = {
  readonly localEventId: EventId;
  readonly localReceiptId?: string;
  readonly encryptedObjectPointer?: string;
};

export type AgentLease = {
  readonly id: LeaseId;
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly deviceId: DeviceId;
  readonly dispatchState: AgentLeaseDispatchState;
  readonly targetDeviceRef?: DeviceId;
  readonly originDeviceRef?: DeviceId;
  readonly writeTargetMode: "direct" | "work-view";
  readonly writeTargetPath: string;
  readonly workViewId?: WorkViewId;
  readonly workViewPath?: string;
  readonly task: string;
  readonly base: AgentLeaseBase;
  readonly baseSnapshotId: SnapshotId;
  readonly sessionState: AgentLeaseSessionState;
  readonly statusSummary: string;
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly updatedAt: string;
};

export const AGENT_TOOL_NAMES = [
  "workspace_status",
  "list_capabilities",
  "resolve_path",
  "list_overlay_changes",
] as const;
export type AgentToolName = (typeof AGENT_TOOL_NAMES)[number];

export type AgentToolCategory = "inspection";

export type AgentCapabilityState = "available" | "degraded" | "unavailable";

export type AgentToolResultOutcome = "allowed" | "denied" | "degraded";

export type AgentToolDenial = {
  readonly code: string;
  readonly safeNextActions: readonly RepairCommand[];
};

export type AgentToolResult = {
  readonly requestId: string;
  readonly leaseId: LeaseId;
  readonly tool: AgentToolName;
  readonly outcome: AgentToolResultOutcome;
  readonly eventId?: EventId;
  readonly receiptId?: string;
  readonly denial?: AgentToolDenial;
  readonly summary: string;
  readonly payload?: Record<string, unknown>;
};

export type AgentCapability = {
  readonly name: AgentToolName;
  readonly category: AgentToolCategory;
  readonly state: AgentCapabilityState;
};

export type AgentCliName = "codex" | "claude" | "cursor";

export type AgentCliCapability = {
  readonly name: AgentCliName;
  readonly available: boolean;
  readonly command?: string;
  readonly supportsPromptFileLaunch: boolean;
  readonly supportsStdinLaunch: boolean;
  readonly supportsCwdSelection: boolean;
  readonly supportsNoninteractiveExecution: boolean;
  readonly supportsReceiptCapture: boolean;
  readonly degradedReason?: string;
};

export type AgentReadinessState = "ready" | "attention" | "limited" | "blocked";

export type AgentReadinessSignal = {
  readonly name: string;
  readonly state: AgentReadinessState;
  readonly summary: string;
  readonly nextAction?: RepairCommand;
};

export type AgentProjectReadiness = {
  readonly state: AgentReadinessState;
  readonly signals: readonly AgentReadinessSignal[];
};

export type AgentStartWork = {
  readonly cwd: string;
  readonly contextCommand: string;
  readonly promptCommand: string;
  readonly safeNextActions: readonly RepairCommand[];
};

export type AgentContextV1 = {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly lease: AgentLease;
  readonly policyVersion: PolicyVersion;
  readonly status: WorkspaceStatus;
  readonly freshness: FreshnessVerdict;
  readonly staleBases?: readonly StaleBaseStatus[];
  readonly writeTargetPath: string;
  readonly workViewPath: string;
  readonly attention: readonly StatusItem[];
  readonly capabilities: readonly AgentCapability[];
  readonly setupReceipts: readonly string[];
  readonly readiness: AgentProjectReadiness;
  readonly startWork: AgentStartWork;
  readonly adapterCapabilities: readonly AgentCliCapability[];
  readonly instructions: readonly string[];
};

export type AgentContextCommandOutput = CommandOutputBase<"agent context"> & {
  readonly context: AgentContextV1;
};

export type AgentLeaseCreateCommandOutput = CommandOutputBase<"agent start"> & {
  readonly lease: AgentLease;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export type AgentPrompt = {
  readonly recipeId: string;
  readonly recipeVersion: number;
  readonly redaction: "applied";
  readonly text: string;
  readonly allowedTools: readonly AgentToolName[];
  readonly adapterCapabilities: readonly AgentCliCapability[];
  readonly instructions: readonly string[];
};

export type AgentPromptCommandOutput = CommandOutputBase<"agent prompt"> & {
  readonly lease: AgentLease;
  readonly prompt: AgentPrompt;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly RepairCommand[];
};

export const AGENT_MCP_GRANTS = ["read"] as const;
export type AgentMcpGrant = (typeof AGENT_MCP_GRANTS)[number];

export type AgentMcpTokenCommandOutput =
  CommandOutputBase<"agent mcp-token"> & {
    readonly workspaceId: WorkspaceId;
    readonly projectId: ProjectId;
    readonly leaseId: LeaseId;
    readonly tokenFile: string;
    readonly grants: readonly AgentMcpGrant[];
    readonly expiresAt: string;
  };
