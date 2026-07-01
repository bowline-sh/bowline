import type { CommandOutputBase } from "./commands";
import type { DeviceId, EventId, LeaseId, PolicyVersion, ProjectId, SnapshotId, WorkspaceId, WorkViewId } from "./ids";
import type { PathClassification } from "./policy";
import type { HydrationBudgetStatus, IndexStatus, SafeAction, StatusItem, WorkspaceStatus } from "./status";

export const AGENT_LEASE_EXECUTION_STATES = [
  "active",
  "blocked",
  "completed",
  "expired",
  "revoked",
] as const;
export type AgentLeaseExecutionState =
  (typeof AGENT_LEASE_EXECUTION_STATES)[number];

export const AGENT_LEASE_OUTPUT_STATES = [
  "empty",
  "dirty",
  "review-ready",
  "accepted",
  "discarded",
  "conflicted",
  "retained",
] as const;
export type AgentLeaseOutputState = (typeof AGENT_LEASE_OUTPUT_STATES)[number];

export const AGENT_LEASE_CLEANUP_STATES = [
  "current",
  "retained",
  "cleanup-pending",
  "cleanup-completed",
  "scrubbed",
] as const;
export type AgentLeaseCleanupState =
  (typeof AGENT_LEASE_CLEANUP_STATES)[number];

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
  readonly writeTargetMode: "direct" | "work-view";
  readonly writeTargetPath: string;
  readonly workViewId?: WorkViewId;
  readonly workViewPath?: string;
  readonly task: string;
  readonly base: AgentLeaseBase;
  readonly baseSnapshotId: SnapshotId;
  readonly executionState: AgentLeaseExecutionState;
  readonly outputState: AgentLeaseOutputState;
  readonly scopes: AgentLeaseScopes;
  readonly hydrateBudgetBytes: number;
  readonly envProfile: AgentEnvProfile;
  readonly envRestrictions: readonly AgentEnvRestriction[];
  readonly outputTarget: AgentOutputTarget;
  readonly audit: AgentAuditPointer;
  readonly cleanupState: AgentLeaseCleanupState;
  readonly statusSummary: string;
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly updatedAt: string;
};

export const AGENT_TOOL_NAMES = [
  "workspace_status",
  "list_capabilities",
  "resolve_path",
  "explain_path_policy",
  "list_attention_items",
  "list_tree_at_snapshot",
  "read_file_at_snapshot",
  "search_workspace",
  "symbol_lookup",
  "request_hydration",
  "get_hydration_status",
  "write_overlay_file",
  "list_overlay_changes",
  "diff_snapshots",
  "run_command_with_receipt",
  "inspect_setup_receipts",
  "propose_policy_change",
  "request_human_decision",
  "publish_overlay_for_review",
  "complete_task",
] as const;
export type AgentToolName = (typeof AGENT_TOOL_NAMES)[number];

export type AgentToolCategory =
  | "inspection"
  | "exploration"
  | "hydration"
  | "write"
  | "execution"
  | "review";

export type AgentCapabilityState = "available" | "degraded" | "unavailable";

export type DegradedExplorationBounds = {
  readonly maxBytes: number;
  readonly maxFiles: number;
  readonly maxDepth: number;
  readonly truncationReason: string;
  readonly continuation?: string;
  readonly safeNextAction: SafeAction;
  readonly indexBackedSearchUnavailable: boolean;
};

export type AgentToolResultOutcome = "allowed" | "denied" | "degraded";

export type AgentToolDenial = {
  readonly code: string;
  readonly safeNextActions: readonly SafeAction[];
};

export type AgentToolResult = {
  readonly requestId: string;
  readonly leaseId: LeaseId;
  readonly tool: AgentToolName;
  readonly outcome: AgentToolResultOutcome;
  readonly eventId?: EventId;
  readonly receiptId?: string;
  readonly denial?: AgentToolDenial;
  readonly degraded?: DegradedExplorationBounds;
  readonly summary: string;
  readonly payload?: Record<string, unknown>;
};

export type AgentCapability = {
  readonly name: AgentToolName;
  readonly category: AgentToolCategory;
  readonly state: AgentCapabilityState;
  readonly bounds?: DegradedExplorationBounds;
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
  readonly nextAction?: SafeAction;
};

export type AgentProjectReadiness = {
  readonly state: AgentReadinessState;
  readonly signals: readonly AgentReadinessSignal[];
};

export type AgentStartWork = {
  readonly cwd: string;
  readonly contextCommand: string;
  readonly promptCommand: string;
  readonly safeNextActions: readonly SafeAction[];
};

export type AgentContextV1 = {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly lease: AgentLease;
  readonly policyVersion: PolicyVersion;
  readonly status: WorkspaceStatus;
  readonly index?: IndexStatus;
  readonly hydrationBudget?: HydrationBudgetStatus;
  readonly writeTargetPath: string;
  readonly workViewPath: string;
  readonly attention: readonly StatusItem[];
  readonly capabilities: readonly AgentCapability[];
  readonly setupReceipts: readonly string[];
  readonly env: AgentEnvProfile;
  readonly scopes: AgentLeaseScopes;
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
  readonly nextActions: readonly SafeAction[];
};

export type AgentPrompt = {
  readonly recipeId: string;
  readonly recipeVersion: number;
  readonly redaction: "applied";
  readonly text: string;
  readonly allowedTools: readonly AgentToolName[];
  readonly outputTarget: AgentOutputTarget;
  readonly adapterCapabilities: readonly AgentCliCapability[];
  readonly instructions: readonly string[];
};

export type AgentPromptCommandOutput = CommandOutputBase<"agent prompt"> & {
  readonly lease: AgentLease;
  readonly prompt: AgentPrompt;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type AgentBudgetCommandOutput = CommandOutputBase<"agent budget"> & {
  readonly lease: AgentLease;
  readonly previousLimitBytes: number;
  readonly addedBytes: number;
  readonly budget: HydrationBudgetStatus;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};
