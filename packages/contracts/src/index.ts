type Brand<TName extends string> = string & { readonly __brand: TName };

export {
  BILLING_PLAN_LIMITS,
  BILLING_PLAN_TIERS,
  BILLING_STORAGE_UNITS,
  FREE_AUTHORIZED_MACHINE_LIMIT,
  FREE_STORAGE_BYTES,
  PRO_STORAGE_BYTES,
  billingPlanLimits,
  totalStoredBytes,
  type BillingPlanLimits,
  type BillingPlanTier,
} from "./billing";

export type WorkspaceId = Brand<"WorkspaceId">;
export type AccountId = Brand<"AccountId">;
export type DeviceId = Brand<"DeviceId">;
export type DeviceApprovalRequestId = Brand<"DeviceApprovalRequestId">;
export type EncryptedDeviceGrantId = Brand<"EncryptedDeviceGrantId">;
export type RecoveryEnvelopeId = Brand<"RecoveryEnvelopeId">;
export type WorkOsUserId = Brand<"WorkOsUserId">;
export type WorkOsOrganizationId = Brand<"WorkOsOrganizationId">;
export type ProjectId = Brand<"ProjectId">;
export type SnapshotId = Brand<"SnapshotId">;
export type ManifestId = Brand<"ManifestId">;
export type PackId = Brand<"PackId">;
export type ContentId = Brand<"ContentId">;
export type LeaseId = Brand<"LeaseId">;
export type WorkViewId = Brand<"WorkViewId">;
export type EventId = Brand<"EventId">;
export type PolicyVersion = Brand<"PolicyVersion">;
export type EnvRecordId = Brand<"EnvRecordId">;

export const CONTRACT_VERSION = 3;

export const SCHEMA_SOURCE_OF_TRUTH = "hand-written-fixtures";

export const STATUS_LEVELS = ["healthy", "attention", "limited"] as const;
export type StatusLevel = (typeof STATUS_LEVELS)[number];

export const PATH_CLASSIFICATIONS = [
  "workspace-sync",
  "project-env",
  "generated",
  "dependency",
  "cache",
  "large-file",
  "secret-looking",
  "local-only",
  "blocked",
] as const;
export type PathClassification = (typeof PATH_CLASSIFICATIONS)[number];

export const MATERIALIZATION_MODES = [
  "workspace-sync",
  "project-env",
  "encrypted-sync",
  "lazy",
  "structure-only",
  "local-regenerate",
  "local-cache",
  "ignore",
  "local-only",
  "blocked",
] as const;
export type MaterializationMode = (typeof MATERIALIZATION_MODES)[number];

export const ACCESS_FLAGS = [
  "human-readable",
  "agent-readable",
  "agent-hidden",
  "lease-only",
] as const;
export type AccessFlag = (typeof ACCESS_FLAGS)[number];

export const EVENT_NAMES = [
  "namespace.created",
  "namespace.moved",
  "namespace.deleted_or_archived",
  "hydration.started",
  "hydration.completed",
  "hydration.blocked",
  "hydration.budget_reserved",
  "hydration.budget_committed",
  "hydration.budget_released",
  "hydration.budget_denied",
  "hydration.budget_override_granted",
  "policy.classified",
  "policy.needs_approval",
  "policy.changed",
  "env.imported",
  "env.materialized",
  "env.revoked",
  "setup.started",
  "setup.completed",
  "setup.blocked",
  "source.stale",
  "work.created",
  "work.updated",
  "work.review_ready",
  "work.accepted",
  "work.discarded",
  "work.restored",
  "work.expired",
  "work.archived",
  "work.cleanup_previewed",
  "work.cleanup_completed",
  "lease.created",
  "lease.updated",
  "lease.expired",
  "lease.completed",
  "lease.blocked",
  "lease.revoked",
  "lease.review_ready",
  "lease.tool_invoked",
  "lease.tool_denied",
  "lease.hydration_requested",
  "lease.cleanup_completed",
  "overlay.changed",
  "publish.requested",
  "conflict.created",
  "conflict.bundle_created",
  "conflict.resolution_proposed",
  "conflict.resolution_accepted",
  "conflict.resolution_rejected",
  "daemon.degraded",
  "daemon.recovered",
  "device.approval_requested",
  "device.approved",
  "device.denied",
  "device.revoked",
  "recovery_key.created",
  "recovery_key.verified",
  "recovery_key.rotated",
  "recovery_key.revoked",
  "auth.login_started",
  "auth.login_completed",
  "index.updated",
  "index.degraded",
  "sync.started",
  "sync.completed",
  "sync.limited",
  "sync.degraded",
  "sync.recovered",
  "watcher.degraded",
  "watcher.recovered",
  "network.offline",
  "network.recovered",
  "metadata.corrupt",
] as const;
export type EventName = (typeof EVENT_NAMES)[number];

export const COMMAND_NAMES = [
  "help",
  "version",
  "contract",
  "unknown",
  "login",
  "approve",
  "revoke",
  "recover",
  "init",
  "setup",
  "prewarm",
  "status",
  "search",
  "symbols",
  "explain",
  "devices",
  "recovery",
  "events",
  "actions",
  "tui",
  "resolve",
  "workon",
  "work",
  "diff",
  "review",
  "accept",
  "discard",
  "restore",
  "cleanup",
  "agent start",
  "agent lease create",
  "agent context",
  "agent prompt",
  "agent publish",
  "agent complete",
  "agent budget",
  "daemon start",
  "daemon stop",
  "daemon status",
  "daemon install",
  "daemon restart",
  "daemon uninstall",
  "diagnostics collect",
  "bootstrap ssh",
  "connect",
] as const;
export type CommandName = (typeof COMMAND_NAMES)[number];

const COMMAND_ERROR_NAMES = COMMAND_NAMES;
type CommandErrorName = (typeof COMMAND_ERROR_NAMES)[number];

export type DeviceFingerprint = Brand<"DeviceFingerprint">;
export type PublicDeviceKey = Brand<"PublicDeviceKey">;

export const DEVICE_APPROVAL_REQUEST_STATES = [
  "pending",
  "approved",
  "denied",
  "expired",
] as const;
export type DeviceApprovalRequestState =
  (typeof DEVICE_APPROVAL_REQUEST_STATES)[number];

export type DeviceApprovalRequest = {
  readonly requestId: DeviceApprovalRequestId;
  readonly workspaceId: WorkspaceId;
  readonly requesterDeviceId: DeviceId;
  readonly deviceName: string;
  readonly platform: DevicePlatform;
  readonly devicePublicKey: PublicDeviceKey;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly matchingCode: string;
  readonly requestedAt: string;
  readonly expiresAt: string;
  readonly state: DeviceApprovalRequestState;
  readonly host?: string;
  readonly root?: string;
};

export type DevicePlatform = "macos" | "linux" | "unknown";

export type AuthorizedDevice = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly authorizedAt: string;
  readonly authorizedByDeviceId?: DeviceId;
};

export type DeviceTrustState =
  | "trusted"
  | "pending"
  | "revoked"
  | "limited"
  | "unavailable"
  | "first-device-setup";

export type DeviceRecord = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly trustState: DeviceTrustState;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly authorizedAt?: string;
  readonly updatedAt: string;
  readonly isCurrentDevice: boolean;
  readonly limitationReason?: string;
};

export type RevokedDevice = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly revokedAt: string;
  readonly revokedByDeviceId: DeviceId;
  readonly reason: string;
};

export type EncryptedDeviceGrantState =
  | "created"
  | "accepted"
  | "expired"
  | "revoked";

export type RecoveryGrantApproverId = Brand<"RecoveryGrantApproverId">;
export type DeviceGrantApproverId = DeviceId | RecoveryGrantApproverId;

export type EncryptedDeviceGrant = {
  readonly grantId: EncryptedDeviceGrantId;
  readonly requestId: DeviceApprovalRequestId;
  readonly workspaceId: WorkspaceId;
  readonly requesterDeviceId: DeviceId;
  readonly requesterDeviceFingerprint: DeviceFingerprint;
  readonly approverDeviceId: DeviceGrantApproverId;
  readonly keyEpoch: number;
  readonly ciphertext: string;
  readonly createdAt: string;
  readonly expiresAt: string;
  readonly state: EncryptedDeviceGrantState;
  readonly acceptedAt?: string;
};

export type RecoveryKeyLifecycle =
  | "missing"
  | "generated-unverified"
  | "active"
  | "rotated"
  | "revoked";

export type RecoveryKeyState = {
  readonly lifecycle: RecoveryKeyLifecycle;
  readonly envelopeId?: RecoveryEnvelopeId;
  readonly fingerprint?: string;
  readonly createdAt?: string;
  readonly verifiedAt?: string;
  readonly rotatedAt?: string;
  readonly revokedAt?: string;
};

export type AccountLoginStatus =
  | "not-logged-in"
  | "login-pending"
  | "account-authenticated"
  | "expired";

export type AccountLoginState = {
  readonly status: AccountLoginStatus;
  readonly accountId?: AccountId;
  readonly workOsUserId?: WorkOsUserId;
  readonly workOsOrganizationId?: WorkOsOrganizationId;
  readonly userCode?: string;
  readonly verificationUri?: string;
  readonly verificationUriComplete?: string;
  readonly pollIntervalSeconds?: number;
  readonly expiresAt?: string;
  readonly authenticatedAt?: string;
};

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

export type CommandOutputBase<TCommand extends string> = {
  readonly contractVersion: typeof CONTRACT_VERSION;
  readonly command: TCommand;
  readonly generatedAt: string;
  readonly workspaceId?: WorkspaceId;
  readonly projectId?: ProjectId;
};

export type CliCommandOption = {
  readonly name: string;
  readonly valueName?: string;
  readonly summary: string;
  readonly required: boolean;
  readonly repeatable: boolean;
};

export type CliCommandExample = {
  readonly command: string;
  readonly summary: string;
};

export type BoundedOutputControls = {
  readonly defaultLimit: number;
  readonly maxLimit: number;
  readonly cursorFormat: string;
  readonly pathPrefix: boolean;
};

export type CliCommandDescriptor = {
  readonly group: string;
  readonly name: string;
  readonly aliases?: readonly string[];
  readonly summary: string;
  readonly usage: string;
  readonly options?: readonly CliCommandOption[];
  readonly examples?: readonly CliCommandExample[];
  readonly jsonOutputType: string;
  readonly sideEffectLevel: string;
  readonly supportsJson: boolean;
  readonly supportsDryRun: boolean;
  readonly supportsIdempotencyKey: boolean;
  readonly boundedOutput?: BoundedOutputControls;
  readonly relatedCommands?: readonly string[];
};

export type CliCommandGroup = {
  readonly name: string;
  readonly commands: readonly string[];
};

export type HelpCommandOutput = CommandOutputBase<"help"> & {
  readonly topic?: string;
  readonly groups: readonly CliCommandGroup[];
  readonly commands: readonly CliCommandDescriptor[];
};

export type VersionCommandOutput = CommandOutputBase<"version"> & {
  readonly cliVersion: string;
  readonly protocol: string;
  readonly protocolVersion: number;
  readonly defaultSocket: string;
  readonly package: string;
};

export type ContractFixtureDescriptor = {
  readonly name: string;
  readonly path: string;
  readonly outputType: string;
};

export type ContractCommandOutput = CommandOutputBase<"contract"> & {
  readonly cliVersion: string;
  readonly protocol: string;
  readonly protocolVersion: number;
  readonly eventSchemaVersion: number;
  readonly package: string;
  readonly packageContractSource: string;
  readonly commandOutputTypes: readonly string[];
  readonly commands: readonly CliCommandDescriptor[];
  readonly fixtures: readonly ContractFixtureDescriptor[];
};

export type DryRunCommandOutput = CommandOutputBase<CommandName> & {
  readonly status: "dry-run";
  readonly allowed: boolean;
  readonly risk: string;
  readonly target: string;
  readonly wouldChange: readonly string[];
  readonly warnings?: readonly string[];
  readonly applyCommand: string;
  readonly nextActions: readonly SafeAction[];
};

export type DaemonProcessOutput = {
  readonly state: string;
  readonly socket: string;
  readonly protocol?: string;
  readonly version?: number;
  readonly daemonVersion?: string;
  readonly pid?: number;
};

export type DaemonServiceState = {
  readonly state: string;
  readonly name?: string;
  readonly unitPath: string;
  readonly unavailableBecause?: string;
};

export type DaemonCommandOutput = CommandOutputBase<
  "daemon start" | "daemon stop"
> & {
  readonly daemon: DaemonProcessOutput;
};

export type DaemonStatusOutput = CommandOutputBase<"daemon status"> & {
  readonly daemon: DaemonProcessOutput;
  readonly sync?: Record<string, unknown>;
  readonly service?: DaemonServiceState;
};

export type DaemonServiceOutput = CommandOutputBase<
  "daemon install" | "daemon restart" | "daemon uninstall"
> & {
  readonly service: DaemonServiceState;
};

export type DiagnosticsCollectCommandOutput =
  CommandOutputBase<"diagnostics collect"> & {
    readonly redactionRules: readonly string[];
    readonly bundle: string;
  };

export type LoginCommandOutput = CommandOutputBase<"login"> & {
  readonly account: AccountLoginState;
  readonly localDevice?: DeviceRecord;
  readonly nextActions: readonly SafeAction[];
};

export type StatusCommandOutput = CommandOutputBase<"status"> & {
  readonly scope?: StatusScope;
  readonly requestedPath?: string;
  readonly resolvedWorkspaceRoot?: string;
  readonly workspaceSummary?: WorkspaceSummary;
  readonly index?: IndexStatus;
  readonly hydrationBudget?: HydrationBudgetStatus;
  readonly hydrationProgress?: readonly HydrationProgress[];
  readonly syncQueue?: SyncQueueStatus;
  readonly status: WorkspaceStatus;
  readonly items: readonly StatusItem[];
  readonly limits: readonly LimitedCapability[];
  readonly eventWatermarks: EventWatermarks;
  readonly nextActions: readonly SafeAction[];
};

export type RootChoiceState =
  | "explicit-existing"
  | "explicit-created"
  | "default-selected"
  | "ambiguous";

export type InitCommandOutput = CommandOutputBase<"login" | "init"> & {
  readonly workspaceId: WorkspaceId;
  readonly root: string;
  readonly rootChoice: RootChoiceState;
  readonly observedOnly: boolean;
  readonly changedWorkspaceFiles: boolean;
  readonly createdRoot: boolean;
  readonly scanSummary: ObservedWorkspaceSummary;
  readonly nonActions: readonly string[];
  readonly nextActions: readonly SafeAction[];
};

export type PrewarmCommandState = "hot" | "setup-blocked" | "no-setup-needed";

export type PrewarmCommandOutcome = {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly projectPath: string;
  readonly state: PrewarmCommandState;
  readonly receiptIds: readonly string[];
  readonly redactedSummary: string;
};

export type PrewarmCommandOutput = CommandOutputBase<"setup" | "prewarm"> & {
  readonly outcome: PrewarmCommandOutcome;
};

export type ExplainCommandOutput = CommandOutputBase<"explain"> & {
  readonly path: string;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access: readonly AccessFlag[];
  readonly matchedRule: string;
  readonly ruleSource: string;
  readonly risk: string;
  readonly observedState: string;
  readonly advisoryNotes?: readonly string[];
  readonly summary: string;
  readonly nextActions: readonly SafeAction[];
};

export type SearchResult = {
  readonly path: string;
  readonly score: number;
  readonly projectId?: ProjectId;
  readonly snapshotId?: SnapshotId;
  readonly lineStart?: number;
  readonly lineEnd?: number;
  readonly snippet?: string;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access: readonly AccessFlag[];
  readonly hydrationState: HydrationState;
};

export type SearchCommandOutput = CommandOutputBase<"search"> & {
  readonly query: string;
  readonly requestedPath?: string;
  readonly index: IndexStatus;
  readonly budget?: HydrationBudgetStatus;
  readonly results: readonly SearchResult[];
  readonly truncated: boolean;
  readonly nextCursor?: string;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export const SYMBOL_KINDS = [
  "function",
  "class",
  "method",
  "variable",
  "constant",
  "type",
  "interface",
  "module",
  "import",
  "export",
  "struct",
  "enum",
  "trait",
] as const;
export type SymbolKind = (typeof SYMBOL_KINDS)[number];

export const SYMBOL_LANGUAGES = [
  "typescript",
  "javascript",
  "python",
  "rust",
  "go",
  "unknown",
] as const;
export type SymbolLanguage = (typeof SYMBOL_LANGUAGES)[number];

export type SymbolResult = {
  readonly name: string;
  readonly kind: SymbolKind;
  readonly language: SymbolLanguage;
  readonly path: string;
  readonly lineStart: number;
  readonly lineEnd: number;
  readonly projectId?: ProjectId;
  readonly snapshotId?: SnapshotId;
  readonly container?: string;
  readonly signature?: string;
  readonly referenceCount?: number;
  readonly classification: PathClassification;
  readonly access: readonly AccessFlag[];
  readonly hydrationState: HydrationState;
};

export type SymbolCommandOutput = CommandOutputBase<"symbols"> & {
  readonly query: string;
  readonly requestedPath?: string;
  readonly index: IndexStatus;
  readonly budget?: HydrationBudgetStatus;
  readonly symbols: readonly SymbolResult[];
  readonly truncated: boolean;
  readonly nextCursor?: string;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type ActionsCommandOutput = CommandOutputBase<"actions"> & {
  readonly scope?: StatusScope;
  readonly status: WorkspaceStatus;
  readonly actions: readonly SafeAction[];
  readonly nonActions: readonly string[];
};

export type DevicesCommandOutput = CommandOutputBase<
  "approve" | "revoke" | "devices"
> & {
  readonly action:
    | "list"
    | "request"
    | "approve"
    | "accept"
    | "deny"
    | "revoke";
  readonly localDevice?: DeviceRecord;
  readonly devices: readonly DeviceRecord[];
  readonly revokedDevices?: readonly RevokedDevice[];
  readonly pendingRequests: readonly DeviceApprovalRequest[];
  readonly createdRequest?: DeviceApprovalRequest;
  readonly approvedDevice?: DeviceRecord;
  readonly deniedRequest?: DeviceApprovalRequest;
  readonly revokedDevice?: RevokedDevice;
  readonly recoveryKey?: RecoveryKeyState;
  readonly nextActions: readonly SafeAction[];
};

export type RecoveryCommandOutput = CommandOutputBase<
  "recover" | "recovery"
> & {
  readonly action: "status" | "create" | "verify" | "rotate" | "revoke" | "use";
  readonly recoveryKey: RecoveryKeyState;
  readonly deviceRequest?: DeviceApprovalRequest;
  readonly encryptedGrant?: EncryptedDeviceGrant;
  readonly nextActions: readonly SafeAction[];
};

export type EventSeverity = "info" | "attention" | "limited";

export type EventSubjectKind =
  | "workspace"
  | "root"
  | "project"
  | "path"
  | "snapshot"
  | "content"
  | "pack"
  | "policy"
  | "env-record"
  | "setup-receipt"
  | "conflict"
  | "work-view"
  | "lease"
  | "overlay"
  | "index"
  | "device"
  | "metadata"
  | "component";

export type EventSubject = {
  readonly kind: EventSubjectKind;
  readonly id: string;
  readonly path?: string;
};

export type EventActorKind = "system" | "daemon" | "device" | "agent" | "user";

export type EventActor = {
  readonly kind: EventActorKind;
  readonly id?: string;
  readonly displayName?: string;
};

export type EventRedaction = {
  readonly status: "not-needed" | "applied";
  readonly rules?: readonly string[];
};

export type WorkspaceEvent = {
  readonly schemaVersion: typeof CONTRACT_VERSION;
  readonly id: EventId;
  readonly name: EventName;
  readonly occurredAt: string;
  readonly severity: EventSeverity;
  readonly summary: string;
  readonly workspaceId: WorkspaceId;
  readonly projectId?: ProjectId;
  readonly path?: string;
  readonly leaseId?: LeaseId;
  readonly deviceId?: DeviceId;
  readonly subject?: EventSubject;
  readonly actor?: EventActor;
  readonly payload?: Record<string, unknown>;
  readonly causationId?: EventId;
  readonly correlationId?: EventId;
  readonly redaction: EventRedaction;
};

export type EventsCommandOutput = CommandOutputBase<"events"> & {
  readonly scope?: StatusScope;
  readonly requestedPath?: string;
  readonly events: readonly WorkspaceEvent[];
  readonly eventWatermarks: EventWatermarks;
};

export type ResolveAction =
  | "list"
  | "copy-prompt"
  | "diff"
  | "agent"
  | "accept"
  | "reject";

export type ResolveAgent = "codex" | "claude" | "cursor";

export type ResolveConflict = {
  readonly id: string;
  readonly state: "unresolved";
  readonly bundlePath: string;
  readonly conflictKind?: string;
  readonly affectedFiles: readonly string[];
  readonly spans?: readonly ResolveConflictSpan[];
  readonly activeView: string;
  readonly hasResolutionOverlay: boolean;
  readonly containsSecrets: boolean;
};

export type ResolveConflictSpan = {
  readonly path: string;
  readonly baseStartLine: number;
  readonly baseEndLine: number;
  readonly localStartLine: number;
  readonly localEndLine: number;
  readonly remoteStartLine: number;
  readonly remoteEndLine: number;
  readonly baseContextHash?: string;
  readonly localContextHash?: string;
  readonly remoteContextHash?: string;
};

export type ResolveAgentOption = {
  readonly name: ResolveAgent;
  readonly command: string;
  readonly capability: AgentCliCapability;
};

export type ResolveAvailableAction = {
  readonly label: string;
  readonly command?: string;
};

export type ResolvePrompt = {
  readonly conflictId: string;
  readonly bundlePath: string;
  readonly resolutionPath: string;
  readonly redaction: "applied";
  readonly text: string;
};

export type ResolveDiff = {
  readonly conflictId: string;
  readonly bundlePath: string;
  readonly redaction: "contents-not-printed";
  readonly affectedFiles: readonly string[];
  readonly text: string;
};

export type ResolveCommandOutput = CommandOutputBase<"resolve"> & {
  readonly projectOrPath: string;
  readonly action: ResolveAction;
  readonly conflicts: readonly ResolveConflict[];
  readonly availableAgents: readonly ResolveAgentOption[];
  readonly availableActions: readonly ResolveAvailableAction[];
  readonly prompt?: ResolvePrompt;
  readonly diff?: ResolveDiff;
  readonly requestedAgent?: ResolveAgent;
  readonly selectedConflictId?: string;
  readonly status: {
    readonly level: StatusLevel;
    readonly summary: string;
  };
  readonly nextActions: readonly ResolveAvailableAction[];
};

export type CommandErrorStatus =
  | "usage-error"
  | "unsupported"
  | "limited"
  | "failed";

export type CommandRecoverability =
  | "retry"
  | "user-action"
  | "unsupported"
  | "none";

export type CommandError = {
  readonly code: string;
  readonly message: string;
  readonly recoverability: CommandRecoverability;
  readonly remediation?: string;
  readonly details?: Record<string, unknown>;
  readonly retryAfterSeconds?: number;
  readonly correlationId?: string;
};

export type CommandErrorOutput = {
  readonly contractVersion: typeof CONTRACT_VERSION;
  readonly command: CommandErrorName;
  readonly generatedAt: string;
  readonly status: CommandErrorStatus;
  readonly error: CommandError;
  readonly nextActions?: readonly SafeAction[];
};

export type WatchFrame =
  | {
      readonly type: "status";
      readonly contractVersion: typeof CONTRACT_VERSION;
      readonly sequence: number;
      readonly generatedAt: string;
      readonly workspaceId: WorkspaceId;
      readonly projectId?: ProjectId;
      readonly status: StatusCommandOutput;
      readonly watermark: EventWatermarks;
      readonly lastEventId?: EventId;
    }
  | {
      readonly type: "event";
      readonly contractVersion: typeof CONTRACT_VERSION;
      readonly sequence: number;
      readonly generatedAt: string;
      readonly workspaceId: WorkspaceId;
      readonly projectId?: ProjectId;
      readonly event: WorkspaceEvent;
      readonly watermark: EventWatermarks;
    }
  | {
      readonly type: "error";
      readonly contractVersion: typeof CONTRACT_VERSION;
      readonly sequence: number;
      readonly generatedAt: string;
      readonly workspaceId: WorkspaceId;
      readonly error: CommandErrorOutput;
    };

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

export type AgentLeaseCreateCommandOutput = CommandOutputBase<
  "agent start" | "agent lease create"
> & {
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

export type BootstrapStepState = "pending" | "completed" | "blocked";

export type BootstrapStep = {
  readonly name: string;
  readonly state: BootstrapStepState;
  readonly summary: string;
};

export type BootstrapSshCommandOutput = CommandOutputBase<
  "connect" | "bootstrap ssh"
> & {
  readonly host: string;
  readonly root: string;
  readonly steps: readonly BootstrapStep[];
  readonly deviceRequest?: DeviceApprovalRequest;
  readonly authorizedDevice?: DeviceRecord;
  readonly remoteDeviceFingerprint?: DeviceFingerprint;
  readonly trusted: boolean;
  readonly secretStore: "os-keychain" | "server-local" | "unavailable";
  readonly sync: "ready" | "prepared" | "blocked";
  readonly nextRequiredPhase?: number;
  readonly remoteStatus: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

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

export type WorkonCommandOutput = CommandOutputBase<"workon"> & {
  readonly action: "created";
  readonly workView: WorkView;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type WorkListCommandOutput = CommandOutputBase<"work"> & {
  readonly action: "listed";
  readonly workspaceId: WorkspaceId;
  readonly workViews: readonly WorkView[];
  readonly includeHidden: boolean;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type WorkDiffCommandOutput = CommandOutputBase<"review" | "diff"> & {
  readonly action: "diffed";
  readonly workView: WorkView;
  readonly changes: readonly WorkDiffEntry[];
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type WorkLifecycleCommandOutput = CommandOutputBase<
  "accept" | "discard" | "restore"
> & {
  readonly action: "accepted" | "review-ready" | "discarded" | "restored";
  readonly workView: WorkView;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export type WorkCleanupCommandOutput = CommandOutputBase<"cleanup"> & {
  readonly action: "cleanup-previewed" | "cleanup-applied";
  readonly workspaceId: WorkspaceId;
  readonly previewedPaths: readonly string[];
  readonly deletedPaths: readonly string[];
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export const SNAPSHOT_KINDS = [
  "base",
  "machine",
  "workspace-head",
  "agent-overlay",
  "conflict",
] as const;
export type SnapshotKind = (typeof SNAPSHOT_KINDS)[number];

export const REF_KINDS = ["workspace", "machine", "project", "lease"] as const;
export type RefKind = (typeof REF_KINDS)[number];

export const NAMESPACE_ENTRY_KINDS = [
  "directory",
  "file",
  "symlink",
  "placeholder",
  "tombstone",
] as const;
export type NamespaceEntryKind = (typeof NAMESPACE_ENTRY_KINDS)[number];

export const HYDRATION_STATES = [
  "local",
  "cold",
  "structure-only",
  "missing",
] as const;
export type HydrationState = (typeof HYDRATION_STATES)[number];

export const CONTENT_STORAGES = ["inline", "packed", "chunked"] as const;
export type ContentStorage = (typeof CONTENT_STORAGES)[number];

export type ContentLocator = {
  readonly contentId: ContentId;
  readonly storage: ContentStorage;
  readonly rawSize: number;
  readonly packId?: PackId;
  readonly offset?: number;
  readonly length?: number;
  readonly chunkIds?: readonly ContentId[];
};

export type NamespaceEntry = {
  readonly path: string;
  readonly kind: NamespaceEntryKind;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access?: readonly AccessFlag[];
  readonly contentId?: ContentId;
  readonly locator?: ContentLocator;
  readonly symlinkTarget?: string;
  readonly byteLen?: number;
  readonly hydrationState: HydrationState;
};

export type WorkspaceRef = {
  readonly name: string;
  readonly targetSnapshotId: SnapshotId;
  readonly kind: RefKind;
};

export type SnapshotManifest = {
  readonly schemaVersion: typeof CONTRACT_VERSION;
  readonly snapshotId: SnapshotId;
  readonly workspaceId: WorkspaceId;
  readonly projectId?: ProjectId;
  readonly kind: SnapshotKind;
  readonly baseSnapshotId?: SnapshotId;
  readonly entries: readonly NamespaceEntry[];
  readonly refs: readonly WorkspaceRef[];
};

export function statusNeedsAttention(status: WorkspaceStatus): boolean {
  return status.level !== "healthy" || status.attentionItems.length > 0;
}

export function isStatusLevel(value: unknown): value is StatusLevel {
  return includesString(STATUS_LEVELS, value);
}

export function isEventName(value: unknown): value is EventName {
  return includesString(EVENT_NAMES, value);
}

export function parseStatusLevel(value: unknown): StatusLevel {
  if (!isStatusLevel(value)) {
    throw new Error(`Unknown status level: ${String(value)}`);
  }

  return value;
}

export function parseEventName(value: unknown): EventName {
  if (!isEventName(value)) {
    throw new Error(`Unknown event name: ${String(value)}`);
  }

  return value;
}

export function isWorkspaceStatus(value: unknown): value is WorkspaceStatus {
  return (
    isRecord(value) &&
    isStatusLevel(value.level) &&
    isStringArray(value.attentionItems)
  );
}

export function isStatusCommandOutput(
  value: unknown,
): value is StatusCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "status" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    isOptionalStatusScope(value.scope) &&
    isOptionalString(value.requestedPath) &&
    isOptionalString(value.resolvedWorkspaceRoot) &&
    isOptionalWorkspaceSummary(value.workspaceSummary) &&
    (value.index === undefined || isIndexStatus(value.index)) &&
    (value.hydrationBudget === undefined ||
      isHydrationBudgetStatus(value.hydrationBudget)) &&
    (value.hydrationProgress === undefined ||
      isHydrationProgressList(value.hydrationProgress)) &&
    (value.syncQueue === undefined || isSyncQueueStatus(value.syncQueue)) &&
    isWorkspaceStatus(value.status) &&
    isStatusItems(value.items) &&
    isLimitedCapabilities(value.limits) &&
    isEventWatermarks(value.eventWatermarks) &&
    isSafeActions(value.nextActions)
  );
}

export function isHelpCommandOutput(
  value: unknown,
): value is HelpCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "help" &&
    typeof value.generatedAt === "string" &&
    isOptionalString(value.topic) &&
    Array.isArray(value.groups) &&
    value.groups.every(isCliCommandGroup) &&
    Array.isArray(value.commands) &&
    value.commands.every(isCliCommandDescriptor)
  );
}

export function isVersionCommandOutput(
  value: unknown,
): value is VersionCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "version" &&
    typeof value.generatedAt === "string" &&
    typeof value.cliVersion === "string" &&
    typeof value.protocol === "string" &&
    isNonNegativeInteger(value.protocolVersion) &&
    typeof value.defaultSocket === "string" &&
    typeof value.package === "string"
  );
}

export function isContractCommandOutput(
  value: unknown,
): value is ContractCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "contract" &&
    typeof value.generatedAt === "string" &&
    typeof value.cliVersion === "string" &&
    typeof value.protocol === "string" &&
    isNonNegativeInteger(value.protocolVersion) &&
    isNonNegativeInteger(value.eventSchemaVersion) &&
    typeof value.package === "string" &&
    typeof value.packageContractSource === "string" &&
    isStringArray(value.commandOutputTypes) &&
    Array.isArray(value.commands) &&
    value.commands.every(isCliCommandDescriptor) &&
    Array.isArray(value.fixtures) &&
    value.fixtures.every(isContractFixtureDescriptor)
  );
}

export function isDryRunCommandOutput(
  value: unknown,
): value is DryRunCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    includesString(COMMAND_NAMES, value.command) &&
    typeof value.generatedAt === "string" &&
    value.status === "dry-run" &&
    typeof value.allowed === "boolean" &&
    typeof value.risk === "string" &&
    typeof value.target === "string" &&
    isStringArray(value.wouldChange) &&
    (value.warnings === undefined || isStringArray(value.warnings)) &&
    typeof value.applyCommand === "string" &&
    isSafeActions(value.nextActions)
  );
}

export function isDaemonCommandOutput(
  value: unknown,
): value is DaemonCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "daemon start" || value.command === "daemon stop") &&
    typeof value.generatedAt === "string" &&
    isDaemonProcessOutput(value.daemon)
  );
}

export function isDaemonStatusOutput(
  value: unknown,
): value is DaemonStatusOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "daemon status" &&
    typeof value.generatedAt === "string" &&
    isDaemonProcessOutput(value.daemon) &&
    (value.sync === undefined || isRecord(value.sync)) &&
    (value.service === undefined || isDaemonServiceState(value.service))
  );
}

export function isDaemonServiceOutput(
  value: unknown,
): value is DaemonServiceOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "daemon install" ||
      value.command === "daemon restart" ||
      value.command === "daemon uninstall") &&
    typeof value.generatedAt === "string" &&
    isDaemonServiceState(value.service)
  );
}

export function isDiagnosticsCollectCommandOutput(
  value: unknown,
): value is DiagnosticsCollectCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "diagnostics collect" &&
    typeof value.generatedAt === "string" &&
    isStringArray(value.redactionRules) &&
    typeof value.bundle === "string"
  );
}

export function isInitCommandOutput(
  value: unknown,
): value is InitCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "login" || value.command === "init") &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.root === "string" &&
    includesString(ROOT_CHOICE_STATES, value.rootChoice) &&
    typeof value.observedOnly === "boolean" &&
    typeof value.changedWorkspaceFiles === "boolean" &&
    typeof value.createdRoot === "boolean" &&
    isObservedWorkspaceSummary(value.scanSummary) &&
    isStringArray(value.nonActions) &&
    isSafeActions(value.nextActions)
  );
}

export function isPrewarmCommandOutput(
  value: unknown,
): value is PrewarmCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "setup" || value.command === "prewarm") &&
    typeof value.generatedAt === "string" &&
    isPrewarmCommandOutcome(value.outcome)
  );
}

export function isExplainCommandOutput(
  value: unknown,
): value is ExplainCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "explain" &&
    typeof value.generatedAt === "string" &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    typeof value.path === "string" &&
    includesString(PATH_CLASSIFICATIONS, value.classification) &&
    includesString(MATERIALIZATION_MODES, value.mode) &&
    isAccessFlags(value.access) &&
    typeof value.matchedRule === "string" &&
    typeof value.ruleSource === "string" &&
    typeof value.risk === "string" &&
    typeof value.observedState === "string" &&
    (value.advisoryNotes === undefined || isStringArray(value.advisoryNotes)) &&
    typeof value.summary === "string" &&
    isSafeActions(value.nextActions)
  );
}

export function isSearchCommandOutput(
  value: unknown,
): value is SearchCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "search" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    typeof value.query === "string" &&
    isOptionalString(value.requestedPath) &&
    isIndexStatus(value.index) &&
    (value.budget === undefined || isHydrationBudgetStatus(value.budget)) &&
    Array.isArray(value.results) &&
    value.results.every(isSearchResult) &&
    typeof value.truncated === "boolean" &&
    isOptionalCursor(value.nextCursor) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isSymbolCommandOutput(
  value: unknown,
): value is SymbolCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "symbols" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    typeof value.query === "string" &&
    isOptionalString(value.requestedPath) &&
    isIndexStatus(value.index) &&
    (value.budget === undefined || isHydrationBudgetStatus(value.budget)) &&
    Array.isArray(value.symbols) &&
    value.symbols.every(isSymbolResult) &&
    typeof value.truncated === "boolean" &&
    isOptionalCursor(value.nextCursor) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isActionsCommandOutput(
  value: unknown,
): value is ActionsCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "actions" &&
    typeof value.generatedAt === "string" &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    isOptionalStatusScope(value.scope) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.actions) &&
    isStringArray(value.nonActions)
  );
}

export function isLoginCommandOutput(
  value: unknown,
): value is LoginCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "login" &&
    typeof value.generatedAt === "string" &&
    isAccountLoginState(value.account) &&
    (value.localDevice === undefined || isDeviceRecord(value.localDevice)) &&
    isSafeActions(value.nextActions)
  );
}

export function isDevicesCommandOutput(
  value: unknown,
): value is DevicesCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "approve" ||
      value.command === "revoke" ||
      value.command === "devices") &&
    typeof value.generatedAt === "string" &&
    includesString(DEVICE_COMMAND_ACTIONS, value.action) &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    (value.localDevice === undefined || isDeviceRecord(value.localDevice)) &&
    Array.isArray(value.devices) &&
    value.devices.every(isDeviceRecord) &&
    (value.revokedDevices === undefined ||
      (Array.isArray(value.revokedDevices) &&
        value.revokedDevices.every(isRevokedDevice))) &&
    Array.isArray(value.pendingRequests) &&
    value.pendingRequests.every(isDeviceApprovalRequest) &&
    (value.createdRequest === undefined ||
      isDeviceApprovalRequest(value.createdRequest)) &&
    (value.approvedDevice === undefined ||
      isDeviceRecord(value.approvedDevice)) &&
    (value.deniedRequest === undefined ||
      isDeviceApprovalRequest(value.deniedRequest)) &&
    (value.revokedDevice === undefined ||
      isRevokedDevice(value.revokedDevice)) &&
    (value.recoveryKey === undefined ||
      isRecoveryKeyState(value.recoveryKey)) &&
    isSafeActions(value.nextActions)
  );
}

export function isRecoveryCommandOutput(
  value: unknown,
): value is RecoveryCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "recover" || value.command === "recovery") &&
    typeof value.generatedAt === "string" &&
    includesString(RECOVERY_COMMAND_ACTIONS, value.action) &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    isRecoveryKeyState(value.recoveryKey) &&
    (value.deviceRequest === undefined ||
      isDeviceApprovalRequest(value.deviceRequest)) &&
    (value.encryptedGrant === undefined ||
      isEncryptedDeviceGrant(value.encryptedGrant)) &&
    value.generatedWords === undefined &&
    isSafeActions(value.nextActions)
  );
}

export function isBootstrapSshCommandOutput(
  value: unknown,
): value is BootstrapSshCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "connect" || value.command === "bootstrap ssh") &&
    typeof value.generatedAt === "string" &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    typeof value.host === "string" &&
    typeof value.root === "string" &&
    Array.isArray(value.steps) &&
    value.steps.every(isBootstrapStep) &&
    (value.deviceRequest === undefined ||
      isDeviceApprovalRequest(value.deviceRequest)) &&
    (value.authorizedDevice === undefined ||
      isDeviceRecord(value.authorizedDevice)) &&
    isOptionalString(value.remoteDeviceFingerprint) &&
    typeof value.trusted === "boolean" &&
    includesString(BOOTSTRAP_SECRET_STORES, value.secretStore) &&
    includesString(BOOTSTRAP_SYNCS, value.sync) &&
    isOptionalNonNegativeNumber(value.nextRequiredPhase) &&
    isWorkspaceStatus(value.remoteStatus) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkonCommandOutput(
  value: unknown,
): value is WorkonCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "workon" &&
    typeof value.generatedAt === "string" &&
    value.action === "created" &&
    isWorkView(value.workView) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkListCommandOutput(
  value: unknown,
): value is WorkListCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "work" &&
    typeof value.generatedAt === "string" &&
    value.action === "listed" &&
    typeof value.workspaceId === "string" &&
    Array.isArray(value.workViews) &&
    value.workViews.every(isWorkView) &&
    typeof value.includeHidden === "boolean" &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkDiffCommandOutput(
  value: unknown,
): value is WorkDiffCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "review" || value.command === "diff") &&
    typeof value.generatedAt === "string" &&
    value.action === "diffed" &&
    isWorkView(value.workView) &&
    Array.isArray(value.changes) &&
    value.changes.every(isWorkDiffEntry) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkLifecycleCommandOutput(
  value: unknown,
): value is WorkLifecycleCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "accept" ||
      value.command === "discard" ||
      value.command === "restore") &&
    typeof value.generatedAt === "string" &&
    ((value.command === "accept" &&
      (value.action === "accepted" || value.action === "review-ready")) ||
      (value.command === "discard" && value.action === "discarded") ||
      (value.command === "restore" && value.action === "restored")) &&
    isWorkView(value.workView) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkCleanupCommandOutput(
  value: unknown,
): value is WorkCleanupCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "cleanup" &&
    typeof value.generatedAt === "string" &&
    (value.action === "cleanup-previewed" ||
      value.action === "cleanup-applied") &&
    typeof value.workspaceId === "string" &&
    isStringArray(value.previewedPaths) &&
    isStringArray(value.deletedPaths) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isWorkspaceEvent(value: unknown): value is WorkspaceEvent {
  return (
    isRecord(value) &&
    value.schemaVersion === CONTRACT_VERSION &&
    typeof value.id === "string" &&
    isEventName(value.name) &&
    typeof value.occurredAt === "string" &&
    includesString(EVENT_SEVERITIES, value.severity) &&
    typeof value.summary === "string" &&
    typeof value.workspaceId === "string" &&
    isOptionalString(value.projectId) &&
    isOptionalString(value.path) &&
    isOptionalString(value.leaseId) &&
    isOptionalString(value.deviceId) &&
    isOptionalEventSubject(value.subject) &&
    isOptionalEventActor(value.actor) &&
    isOptionalPayload(value.payload) &&
    isOptionalString(value.causationId) &&
    isOptionalString(value.correlationId) &&
    isEventRedaction(value.redaction)
  );
}

export function isEventsCommandOutput(
  value: unknown,
): value is EventsCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "events" &&
    typeof value.generatedAt === "string" &&
    isOptionalString(value.workspaceId) &&
    isOptionalString(value.projectId) &&
    isOptionalStatusScope(value.scope) &&
    isOptionalString(value.requestedPath) &&
    Array.isArray(value.events) &&
    value.events.every(isWorkspaceEvent) &&
    isEventWatermarks(value.eventWatermarks)
  );
}

export function isResolveCommandOutput(
  value: unknown,
): value is ResolveCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "resolve" &&
    typeof value.generatedAt === "string" &&
    typeof value.projectOrPath === "string" &&
    includesString(RESOLVE_ACTIONS, value.action) &&
    Array.isArray(value.conflicts) &&
    value.conflicts.every(isResolveConflict) &&
    Array.isArray(value.availableAgents) &&
    value.availableAgents.every(isResolveAgentOption) &&
    isResolveAvailableActions(value.availableActions) &&
    (value.prompt === undefined || isResolvePrompt(value.prompt)) &&
    (value.diff === undefined || isResolveDiff(value.diff)) &&
    (value.requestedAgent === undefined ||
      includesString(RESOLVE_AGENTS, value.requestedAgent)) &&
    isOptionalString(value.selectedConflictId) &&
    isResolveStatus(value.status) &&
    isResolveAvailableActions(value.nextActions)
  );
}

export function isAgentLeaseCreateCommandOutput(
  value: unknown,
): value is AgentLeaseCreateCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    (value.command === "agent start" ||
      value.command === "agent lease create") &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    isAgentLease(value.lease) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isAgentContextCommandOutput(
  value: unknown,
): value is AgentContextCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "agent context" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    isAgentContextV1(value.context)
  );
}

export function isAgentPromptCommandOutput(
  value: unknown,
): value is AgentPromptCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "agent prompt" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    isAgentLease(value.lease) &&
    isAgentPrompt(value.prompt) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isAgentBudgetCommandOutput(
  value: unknown,
): value is AgentBudgetCommandOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    value.command === "agent budget" &&
    typeof value.generatedAt === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    isAgentLease(value.lease) &&
    isNonNegativeNumber(value.previousLimitBytes) &&
    isNonNegativeNumber(value.addedBytes) &&
    isHydrationBudgetStatus(value.budget) &&
    isWorkspaceStatus(value.status) &&
    isSafeActions(value.nextActions)
  );
}

export function isAgentToolResult(value: unknown): value is AgentToolResult {
  return (
    isRecord(value) &&
    typeof value.requestId === "string" &&
    typeof value.leaseId === "string" &&
    includesString(AGENT_TOOL_NAMES, value.tool) &&
    (value.outcome === "allowed" ||
      value.outcome === "denied" ||
      value.outcome === "degraded") &&
    isOptionalString(value.eventId) &&
    isOptionalString(value.receiptId) &&
    (value.denial === undefined || isAgentToolDenial(value.denial)) &&
    (value.degraded === undefined ||
      isDegradedExplorationBounds(value.degraded)) &&
    typeof value.summary === "string" &&
    isOptionalPayload(value.payload)
  );
}

export function isWatchFrame(value: unknown): value is WatchFrame {
  if (!isRecord(value)) return false;
  if (value.contractVersion !== CONTRACT_VERSION) return false;
  if (typeof value.sequence !== "number" || value.sequence < 0) return false;
  if (typeof value.generatedAt !== "string") return false;
  if (typeof value.workspaceId !== "string") return false;

  if (value.type === "status") {
    return (
      isOptionalString(value.projectId) &&
      isStatusCommandOutput(value.status) &&
      isEventWatermarks(value.watermark) &&
      isOptionalString(value.lastEventId)
    );
  }

  if (value.type === "event") {
    return (
      isOptionalString(value.projectId) &&
      isWorkspaceEvent(value.event) &&
      isEventWatermarks(value.watermark)
    );
  }

  if (value.type === "error") {
    return isCommandErrorOutput(value.error);
  }

  return false;
}

export function isCommandErrorOutput(
  value: unknown,
): value is CommandErrorOutput {
  return (
    isRecord(value) &&
    value.contractVersion === CONTRACT_VERSION &&
    includesString(COMMAND_ERROR_NAMES, value.command) &&
    typeof value.generatedAt === "string" &&
    includesString(COMMAND_ERROR_STATUSES, value.status) &&
    isCommandError(value.error) &&
    (value.nextActions === undefined || isSafeActions(value.nextActions))
  );
}

function isCliCommandGroup(value: unknown): value is CliCommandGroup {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    isStringArray(value.commands)
  );
}

function isCliCommandOption(value: unknown): value is CliCommandOption {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    isOptionalString(value.valueName) &&
    typeof value.summary === "string" &&
    typeof value.required === "boolean" &&
    typeof value.repeatable === "boolean"
  );
}

function isCliCommandExample(value: unknown): value is CliCommandExample {
  return (
    isRecord(value) &&
    typeof value.command === "string" &&
    typeof value.summary === "string"
  );
}

function isBoundedOutputControls(
  value: unknown,
): value is BoundedOutputControls {
  return (
    isRecord(value) &&
    isNonNegativeInteger(value.defaultLimit) &&
    isNonNegativeInteger(value.maxLimit) &&
    typeof value.cursorFormat === "string" &&
    typeof value.pathPrefix === "boolean"
  );
}

function isCliCommandDescriptor(value: unknown): value is CliCommandDescriptor {
  return (
    isRecord(value) &&
    typeof value.group === "string" &&
    typeof value.name === "string" &&
    (value.aliases === undefined || isStringArray(value.aliases)) &&
    typeof value.summary === "string" &&
    typeof value.usage === "string" &&
    (value.options === undefined ||
      (Array.isArray(value.options) &&
        value.options.every(isCliCommandOption))) &&
    (value.examples === undefined ||
      (Array.isArray(value.examples) &&
        value.examples.every(isCliCommandExample))) &&
    typeof value.jsonOutputType === "string" &&
    typeof value.sideEffectLevel === "string" &&
    typeof value.supportsJson === "boolean" &&
    typeof value.supportsDryRun === "boolean" &&
    typeof value.supportsIdempotencyKey === "boolean" &&
    (value.boundedOutput === undefined ||
      isBoundedOutputControls(value.boundedOutput)) &&
    (value.relatedCommands === undefined ||
      isStringArray(value.relatedCommands))
  );
}

function isContractFixtureDescriptor(
  value: unknown,
): value is ContractFixtureDescriptor {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    typeof value.path === "string" &&
    typeof value.outputType === "string"
  );
}

function isPrewarmCommandOutcome(
  value: unknown,
): value is PrewarmCommandOutcome {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    typeof value.projectPath === "string" &&
    includesString(PREWARM_COMMAND_STATES, value.state) &&
    isStringArray(value.receiptIds) &&
    typeof value.redactedSummary === "string"
  );
}

function isAgentLease(value: unknown): value is AgentLease {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    typeof value.deviceId === "string" &&
    (value.writeTargetMode === "direct" ||
      value.writeTargetMode === "work-view") &&
    typeof value.writeTargetPath === "string" &&
    (value.workViewId === undefined || typeof value.workViewId === "string") &&
    (value.workViewPath === undefined ||
      typeof value.workViewPath === "string") &&
    (value.writeTargetMode !== "work-view" ||
      (typeof value.workViewId === "string" &&
        typeof value.workViewPath === "string")) &&
    typeof value.task === "string" &&
    (value.base === "latest-workspace" || value.base === "latest:main") &&
    typeof value.baseSnapshotId === "string" &&
    includesString(AGENT_LEASE_EXECUTION_STATES, value.executionState) &&
    includesString(AGENT_LEASE_OUTPUT_STATES, value.outputState) &&
    isAgentLeaseScopes(value.scopes) &&
    isNonNegativeNumber(value.hydrateBudgetBytes) &&
    isAgentEnvProfile(value.envProfile) &&
    Array.isArray(value.envRestrictions) &&
    value.envRestrictions.every(isAgentEnvRestriction) &&
    isAgentOutputTarget(value.outputTarget) &&
    isAgentAuditPointer(value.audit) &&
    includesString(AGENT_LEASE_CLEANUP_STATES, value.cleanupState) &&
    typeof value.statusSummary === "string" &&
    typeof value.expiresAt === "string" &&
    typeof value.createdAt === "string" &&
    typeof value.updatedAt === "string"
  );
}

function isAgentContextV1(value: unknown): value is AgentContextV1 {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    isAgentLease(value.lease) &&
    typeof value.policyVersion === "string" &&
    isWorkspaceStatus(value.status) &&
    (value.index === undefined || isIndexStatus(value.index)) &&
    (value.hydrationBudget === undefined ||
      isHydrationBudgetStatus(value.hydrationBudget)) &&
    typeof value.writeTargetPath === "string" &&
    typeof value.workViewPath === "string" &&
    isStatusItems(value.attention) &&
    Array.isArray(value.capabilities) &&
    value.capabilities.every(isAgentCapability) &&
    isStringArray(value.setupReceipts) &&
    isAgentEnvProfile(value.env) &&
    isAgentLeaseScopes(value.scopes) &&
    isAgentProjectReadiness(value.readiness) &&
    isAgentStartWork(value.startWork) &&
    Array.isArray(value.adapterCapabilities) &&
    value.adapterCapabilities.every(isAgentCliCapability) &&
    isStringArray(value.instructions)
  );
}

function isAgentPrompt(value: unknown): value is AgentPrompt {
  return (
    isRecord(value) &&
    typeof value.recipeId === "string" &&
    isNonNegativeNumber(value.recipeVersion) &&
    value.redaction === "applied" &&
    typeof value.text === "string" &&
    Array.isArray(value.allowedTools) &&
    value.allowedTools.every((tool) =>
      includesString(AGENT_TOOL_NAMES, tool),
    ) &&
    isAgentOutputTarget(value.outputTarget) &&
    Array.isArray(value.adapterCapabilities) &&
    value.adapterCapabilities.every(isAgentCliCapability) &&
    isStringArray(value.instructions)
  );
}

function isAgentLeaseScopes(value: unknown): value is AgentLeaseScopes {
  return (
    isRecord(value) &&
    isAgentLeaseScope(value.read) &&
    isAgentLeaseScope(value.write)
  );
}

function isAgentLeaseScope(value: unknown): value is AgentLeaseScope {
  return (
    isRecord(value) &&
    isStringArray(value.roots) &&
    (value.classifications === undefined ||
      (Array.isArray(value.classifications) &&
        value.classifications.every((item) =>
          includesString(PATH_CLASSIFICATIONS, item),
        ))) &&
    isOptionalNonNegativeNumber(value.maxBytesPerRead) &&
    isOptionalNonNegativeNumber(value.maxFilesPerRequest) &&
    isOptionalNonNegativeNumber(value.maxDepth)
  );
}

function isAgentEnvRestriction(value: unknown): value is AgentEnvRestriction {
  return (
    isRecord(value) &&
    (value.kind === "allowlist" ||
      value.kind === "blocked-secret" ||
      value.kind === "grant-required") &&
    typeof value.key === "string" &&
    isOptionalString(value.reason) &&
    isOptionalString(value.grantId)
  );
}

function isAgentEnvProfile(value: unknown): value is AgentEnvProfile {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    (value.materialization === "lease-work-view" ||
      value.materialization === "project-path" ||
      value.materialization === "unavailable") &&
    isStringArray(value.availableKeys) &&
    Array.isArray(value.restrictions) &&
    value.restrictions.every(isAgentEnvRestriction) &&
    isStringArray(value.grantIds)
  );
}

function isAgentOutputTarget(value: unknown): value is AgentOutputTarget {
  return (
    isRecord(value) &&
    ((value.kind === "real-project" &&
      value.workViewId === undefined &&
      typeof value.path === "string") ||
      (value.kind === "work-view" &&
        typeof value.workViewId === "string" &&
        typeof value.path === "string"))
  );
}

function isAgentAuditPointer(value: unknown): value is AgentAuditPointer {
  return (
    isRecord(value) &&
    typeof value.localEventId === "string" &&
    isOptionalString(value.localReceiptId) &&
    isOptionalString(value.encryptedObjectPointer)
  );
}

function isAgentCapability(value: unknown): value is AgentCapability {
  return (
    isRecord(value) &&
    includesString(AGENT_TOOL_NAMES, value.name) &&
    (value.category === "inspection" ||
      value.category === "exploration" ||
      value.category === "hydration" ||
      value.category === "write" ||
      value.category === "execution" ||
      value.category === "review") &&
    (value.state === "available" ||
      value.state === "degraded" ||
      value.state === "unavailable") &&
    (value.bounds === undefined || isDegradedExplorationBounds(value.bounds))
  );
}

function isAgentCliCapability(value: unknown): value is AgentCliCapability {
  return (
    isRecord(value) &&
    includesString(AGENT_CLI_NAMES, value.name) &&
    typeof value.available === "boolean" &&
    isOptionalString(value.command) &&
    typeof value.supportsPromptFileLaunch === "boolean" &&
    typeof value.supportsStdinLaunch === "boolean" &&
    typeof value.supportsCwdSelection === "boolean" &&
    typeof value.supportsNoninteractiveExecution === "boolean" &&
    typeof value.supportsReceiptCapture === "boolean" &&
    isOptionalString(value.degradedReason)
  );
}

function isAgentProjectReadiness(
  value: unknown,
): value is AgentProjectReadiness {
  return (
    isRecord(value) &&
    includesString(AGENT_READINESS_STATES, value.state) &&
    Array.isArray(value.signals) &&
    value.signals.every(isAgentReadinessSignal)
  );
}

function isAgentReadinessSignal(value: unknown): value is AgentReadinessSignal {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    includesString(AGENT_READINESS_STATES, value.state) &&
    typeof value.summary === "string" &&
    (value.nextAction === undefined || isSafeAction(value.nextAction))
  );
}

function isAgentStartWork(value: unknown): value is AgentStartWork {
  return (
    isRecord(value) &&
    typeof value.cwd === "string" &&
    typeof value.contextCommand === "string" &&
    typeof value.promptCommand === "string" &&
    isSafeActions(value.safeNextActions)
  );
}

function isDegradedExplorationBounds(
  value: unknown,
): value is DegradedExplorationBounds {
  return (
    isRecord(value) &&
    isNonNegativeNumber(value.maxBytes) &&
    isNonNegativeNumber(value.maxFiles) &&
    isNonNegativeNumber(value.maxDepth) &&
    typeof value.truncationReason === "string" &&
    isOptionalString(value.continuation) &&
    isSafeAction(value.safeNextAction) &&
    typeof value.indexBackedSearchUnavailable === "boolean"
  );
}

function isAgentToolDenial(value: unknown): value is AgentToolDenial {
  return (
    isRecord(value) &&
    typeof value.code === "string" &&
    isSafeActions(value.safeNextActions)
  );
}

function isDaemonProcessOutput(value: unknown): value is DaemonProcessOutput {
  return (
    isRecord(value) &&
    typeof value.state === "string" &&
    typeof value.socket === "string" &&
    isOptionalString(value.protocol) &&
    (value.version === undefined || isNonNegativeInteger(value.version)) &&
    isOptionalString(value.daemonVersion) &&
    (value.pid === undefined || isNonNegativeInteger(value.pid))
  );
}

function isDaemonServiceState(value: unknown): value is DaemonServiceState {
  return (
    isRecord(value) &&
    typeof value.state === "string" &&
    isOptionalString(value.name) &&
    typeof value.unitPath === "string" &&
    isOptionalString(value.unavailableBecause)
  );
}

function isIndexStatus(value: unknown): value is IndexStatus {
  if (!isRecord(value)) return false;
  if (!includesString(INDEX_STATES, value.state)) return false;
  if (
    value.source !== "local" &&
    value.source !== "encrypted-index-pack" &&
    value.source !== "none"
  ) {
    return false;
  }
  if (!isOptionalString(value.indexedAt)) return false;
  if (!isOptionalString(value.updatedAt)) return false;
  if (!isOptionalString(value.snapshotId)) return false;
  if (
    value.indexPackObjectKey !== undefined &&
    (typeof value.indexPackObjectKey !== "string" ||
      !/^indexes_ix_[a-f0-9]{16,80}$/u.test(value.indexPackObjectKey))
  ) {
    return false;
  }
  if (!isNonNegativeNumber(value.pathCount)) return false;
  if (!isNonNegativeNumber(value.fileCount)) return false;
  if (!isNonNegativeNumber(value.indexedBytes)) return false;
  if (!isOptionalNonNegativeNumber(value.pendingPathCount)) return false;
  if (
    value.degradedReason !== undefined &&
    !includesString(INDEX_DEGRADED_REASONS, value.degradedReason)
  ) {
    return false;
  }
  if (typeof value.summary !== "string") return false;
  if (value.nextAction !== undefined && !isSafeAction(value.nextAction)) {
    return false;
  }

  if (value.state === "degraded") {
    return typeof value.degradedReason === "string";
  }
  return true;
}

function isHydrationBudgetStatus(
  value: unknown,
): value is HydrationBudgetStatus {
  return (
    isRecord(value) &&
    includesString(HYDRATION_BUDGET_STATES, value.state) &&
    isNonNegativeNumber(value.limitBytes) &&
    isNonNegativeNumber(value.usedBytes) &&
    isNonNegativeNumber(value.reservedBytes) &&
    isNonNegativeNumber(value.remainingBytes) &&
    (value.scope === "lease" ||
      value.scope === "project" ||
      value.scope === "workspace") &&
    isOptionalString(value.leaseId) &&
    isOptionalString(value.projectId) &&
    isOptionalString(value.resetAt) &&
    (value.nextAction === undefined || isSafeAction(value.nextAction))
  );
}

function isHydrationProgress(value: unknown): value is HydrationProgress {
  return (
    isRecord(value) &&
    isOptionalString(value.projectId) &&
    isNonNegativeNumber(value.bytesDone) &&
    isNonNegativeNumber(value.bytesRemaining) &&
    typeof value.cause === "string"
  );
}

function isHydrationProgressList(
  value: unknown,
): value is readonly HydrationProgress[] {
  return Array.isArray(value) && value.every(isHydrationProgress);
}

function isSearchResult(value: unknown): value is SearchResult {
  return (
    isRecord(value) &&
    typeof value.path === "string" &&
    isNonNegativeNumber(value.score) &&
    isOptionalString(value.projectId) &&
    isOptionalString(value.snapshotId) &&
    isOptionalNonNegativeNumber(value.lineStart) &&
    isOptionalNonNegativeNumber(value.lineEnd) &&
    isOptionalString(value.snippet) &&
    includesString(PATH_CLASSIFICATIONS, value.classification) &&
    includesString(MATERIALIZATION_MODES, value.mode) &&
    isAccessFlags(value.access) &&
    includesString(HYDRATION_STATES, value.hydrationState)
  );
}

function isSymbolResult(value: unknown): value is SymbolResult {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    includesString(SYMBOL_KINDS, value.kind) &&
    includesString(SYMBOL_LANGUAGES, value.language) &&
    typeof value.path === "string" &&
    isNonNegativeNumber(value.lineStart) &&
    isNonNegativeNumber(value.lineEnd) &&
    isOptionalString(value.projectId) &&
    isOptionalString(value.snapshotId) &&
    isOptionalString(value.container) &&
    isOptionalString(value.signature) &&
    isOptionalNonNegativeNumber(value.referenceCount) &&
    includesString(PATH_CLASSIFICATIONS, value.classification) &&
    isAccessFlags(value.access) &&
    includesString(HYDRATION_STATES, value.hydrationState)
  );
}

export function isSnapshotManifest(value: unknown): value is SnapshotManifest {
  return (
    isRecord(value) &&
    value.schemaVersion === CONTRACT_VERSION &&
    typeof value.snapshotId === "string" &&
    typeof value.workspaceId === "string" &&
    isOptionalString(value.projectId) &&
    includesString(SNAPSHOT_KINDS, value.kind) &&
    isOptionalString(value.baseSnapshotId) &&
    Array.isArray(value.entries) &&
    value.entries.every(isNamespaceEntry) &&
    Array.isArray(value.refs) &&
    value.refs.every(isWorkspaceRef)
  );
}

function includesString<const TValues extends readonly string[]>(
  values: TValues,
  value: unknown,
): value is TValues[number] {
  return typeof value === "string" && values.includes(value);
}

const EVENT_SEVERITIES = [
  "info",
  "attention",
  "limited",
] as const satisfies readonly EventSeverity[];

const EVENT_SUBJECT_KINDS = [
  "workspace",
  "root",
  "project",
  "path",
  "snapshot",
  "content",
  "pack",
  "policy",
  "env-record",
  "setup-receipt",
  "conflict",
  "work-view",
  "lease",
  "overlay",
  "index",
  "device",
  "metadata",
  "component",
] as const satisfies readonly EventSubjectKind[];

const EVENT_ACTOR_KINDS = [
  "system",
  "daemon",
  "device",
  "agent",
  "user",
] as const satisfies readonly EventActorKind[];

const COMMAND_ERROR_STATUSES = [
  "usage-error",
  "unsupported",
  "limited",
  "failed",
] as const satisfies readonly CommandErrorStatus[];

const COMMAND_RECOVERABILITIES = [
  "retry",
  "user-action",
  "unsupported",
  "none",
] as const satisfies readonly CommandRecoverability[];

const ROOT_CHOICE_STATES = [
  "explicit-existing",
  "explicit-created",
  "default-selected",
  "ambiguous",
] as const satisfies readonly RootChoiceState[];

const PREWARM_COMMAND_STATES = [
  "hot",
  "setup-blocked",
  "no-setup-needed",
] as const satisfies readonly PrewarmCommandState[];

const DEVICE_PLATFORMS = [
  "macos",
  "linux",
  "unknown",
] as const satisfies readonly DevicePlatform[];

const DEVICE_TRUST_STATES = [
  "trusted",
  "pending",
  "revoked",
  "limited",
  "unavailable",
  "first-device-setup",
] as const satisfies readonly DeviceTrustState[];

const DEVICE_COMMAND_ACTIONS = [
  "list",
  "request",
  "approve",
  "accept",
  "deny",
  "revoke",
] as const satisfies readonly DevicesCommandOutput["action"][];

const RECOVERY_COMMAND_ACTIONS = [
  "status",
  "create",
  "verify",
  "rotate",
  "revoke",
  "use",
] as const satisfies readonly RecoveryCommandOutput["action"][];

const RESOLVE_ACTIONS = [
  "list",
  "copy-prompt",
  "diff",
  "agent",
  "accept",
  "reject",
] as const satisfies readonly ResolveAction[];

const RESOLVE_AGENTS = [
  "codex",
  "claude",
  "cursor",
] as const satisfies readonly ResolveAgent[];

const AGENT_CLI_NAMES = [
  "codex",
  "claude",
  "cursor",
] as const satisfies readonly AgentCliName[];

const AGENT_READINESS_STATES = [
  "ready",
  "attention",
  "limited",
  "blocked",
] as const satisfies readonly AgentReadinessState[];

const ENCRYPTED_DEVICE_GRANT_STATES = [
  "created",
  "accepted",
  "expired",
  "revoked",
] as const satisfies readonly EncryptedDeviceGrantState[];

const RECOVERY_KEY_LIFECYCLES = [
  "missing",
  "generated-unverified",
  "active",
  "rotated",
  "revoked",
] as const satisfies readonly RecoveryKeyLifecycle[];

const ACCOUNT_LOGIN_STATUSES = [
  "not-logged-in",
  "login-pending",
  "account-authenticated",
  "expired",
] as const satisfies readonly AccountLoginStatus[];

const BOOTSTRAP_STEP_STATES = [
  "pending",
  "completed",
  "blocked",
] as const satisfies readonly BootstrapStepState[];

const BOOTSTRAP_SECRET_STORES = [
  "os-keychain",
  "server-local",
  "unavailable",
] as const satisfies readonly BootstrapSshCommandOutput["secretStore"][];

const BOOTSTRAP_SYNCS = [
  "ready",
  "prepared",
  "blocked",
] as const satisfies readonly BootstrapSshCommandOutput["sync"][];

const INDEX_DEGRADED_REASONS = [
  "missing",
  "corrupt",
  "unsupported",
  "policy-limited",
  "rebuild-failed",
] as const satisfies readonly IndexDegradedReason[];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStringArray(value: unknown): value is readonly string[] {
  return (
    Array.isArray(value) && value.every((item) => typeof item === "string")
  );
}

function isStatusItems(value: unknown): value is readonly StatusItem[] {
  return Array.isArray(value) && value.every(isStatusItem);
}

function isStatusItem(value: unknown): value is StatusItem {
  if (!isRecord(value)) return false;
  if (!includesString(STATUS_ITEM_KINDS, value.kind)) return false;
  if (typeof value.summary !== "string") return false;
  if (!isOptionalStatusSubject(value.subject)) return false;
  if (!isOptionalString(value.path)) return false;
  if (!isOptionalString(value.eventId)) return false;
  if (!isOptionalString(value.deviceId)) return false;
  if (!isOptionalString(value.leaseId)) return false;
  if (!isOptionalString(value.projectId)) return false;
  if (!isOptionalString(value.snapshotId)) return false;
  if (!isOptionalString(value.policyVersion)) return false;
  if (!isOptionalString(value.envRecordId)) return false;
  if (
    value.classification !== undefined &&
    !includesString(PATH_CLASSIFICATIONS, value.classification)
  ) {
    return false;
  }
  if (
    value.mode !== undefined &&
    !includesString(MATERIALIZATION_MODES, value.mode)
  ) {
    return false;
  }
  if (value.access !== undefined && !isAccessFlags(value.access)) {
    return false;
  }
  if (value.eventName !== undefined && !isEventName(value.eventName)) {
    return false;
  }

  return true;
}

const STATUS_ITEM_KINDS = [
  "continuity",
  "policy",
  "device",
  "conflict",
  "work-view",
  "lease",
  "watcher",
  "env",
  "hydration",
  "source",
  "setup",
  "metadata",
  "materialization",
  "network",
  "index",
] as const satisfies readonly StatusItemKind[];

const STATUS_SUBJECT_KINDS = [
  "workspace",
  "root",
  "project",
  "path",
  "snapshot",
  "env-record",
  "policy",
  "setup-receipt",
  "conflict",
  "work-view",
  "hydration",
  "lease",
  "overlay",
  "device",
  "device-approval-request",
  "metadata",
  "component",
  "index",
] as const satisfies readonly StatusSubjectKind[];

function isOptionalStatusSubject(
  value: unknown,
): value is StatusSubject | undefined {
  if (value === undefined) return true;
  return (
    isRecord(value) &&
    includesString(STATUS_SUBJECT_KINDS, value.kind) &&
    typeof value.id === "string" &&
    isOptionalString(value.path)
  );
}

function isOptionalStatusScope(
  value: unknown,
): value is StatusScope | undefined {
  return value === undefined || includesString(STATUS_SCOPES, value);
}

function isOptionalEventSubject(
  value: unknown,
): value is EventSubject | undefined {
  if (value === undefined) return true;
  return (
    isRecord(value) &&
    includesString(EVENT_SUBJECT_KINDS, value.kind) &&
    typeof value.id === "string" &&
    isOptionalString(value.path)
  );
}

function isOptionalEventActor(value: unknown): value is EventActor | undefined {
  if (value === undefined) return true;
  return (
    isRecord(value) &&
    includesString(EVENT_ACTOR_KINDS, value.kind) &&
    isOptionalString(value.id) &&
    isOptionalString(value.displayName)
  );
}

function isOptionalPayload(
  value: unknown,
): value is Record<string, unknown> | undefined {
  return value === undefined || isRecord(value);
}

function isEventRedaction(value: unknown): value is EventRedaction {
  return (
    isRecord(value) &&
    (value.status === "not-needed" || value.status === "applied") &&
    (value.rules === undefined || isStringArray(value.rules))
  );
}

function isCommandError(value: unknown): value is CommandError {
  return (
    isRecord(value) &&
    typeof value.code === "string" &&
    typeof value.message === "string" &&
    includesString(COMMAND_RECOVERABILITIES, value.recoverability) &&
    isOptionalString(value.remediation) &&
    (value.details === undefined || isRecord(value.details)) &&
    isOptionalNonNegativeNumber(value.retryAfterSeconds) &&
    isOptionalString(value.correlationId)
  );
}

function isAccountLoginState(value: unknown): value is AccountLoginState {
  return (
    isRecord(value) &&
    includesString(ACCOUNT_LOGIN_STATUSES, value.status) &&
    isOptionalString(value.accountId) &&
    isOptionalString(value.workOsUserId) &&
    isOptionalString(value.workOsOrganizationId) &&
    isOptionalString(value.userCode) &&
    isOptionalString(value.verificationUri) &&
    isOptionalString(value.verificationUriComplete) &&
    isOptionalNonNegativeNumber(value.pollIntervalSeconds) &&
    isOptionalString(value.expiresAt) &&
    isOptionalString(value.authenticatedAt)
  );
}

function isDeviceApprovalRequest(
  value: unknown,
): value is DeviceApprovalRequest {
  return (
    isRecord(value) &&
    typeof value.requestId === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.requesterDeviceId === "string" &&
    typeof value.deviceName === "string" &&
    includesString(DEVICE_PLATFORMS, value.platform) &&
    typeof value.devicePublicKey === "string" &&
    typeof value.deviceFingerprint === "string" &&
    typeof value.matchingCode === "string" &&
    typeof value.requestedAt === "string" &&
    typeof value.expiresAt === "string" &&
    includesString(DEVICE_APPROVAL_REQUEST_STATES, value.state) &&
    isOptionalString(value.host) &&
    isOptionalString(value.root)
  );
}

function isDeviceRecord(value: unknown): value is DeviceRecord {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.name === "string" &&
    typeof value.workspaceId === "string" &&
    includesString(DEVICE_PLATFORMS, value.platform) &&
    includesString(DEVICE_TRUST_STATES, value.trustState) &&
    typeof value.deviceFingerprint === "string" &&
    isOptionalString(value.authorizedAt) &&
    typeof value.updatedAt === "string" &&
    typeof value.isCurrentDevice === "boolean" &&
    isOptionalString(value.limitationReason)
  );
}

function isRevokedDevice(value: unknown): value is RevokedDevice {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.name === "string" &&
    typeof value.workspaceId === "string" &&
    includesString(DEVICE_PLATFORMS, value.platform) &&
    typeof value.deviceFingerprint === "string" &&
    typeof value.revokedAt === "string" &&
    typeof value.revokedByDeviceId === "string" &&
    typeof value.reason === "string"
  );
}

function isEncryptedDeviceGrant(value: unknown): value is EncryptedDeviceGrant {
  return (
    isRecord(value) &&
    typeof value.grantId === "string" &&
    typeof value.requestId === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.requesterDeviceId === "string" &&
    typeof value.requesterDeviceFingerprint === "string" &&
    typeof value.approverDeviceId === "string" &&
    isNonNegativeNumber(value.keyEpoch) &&
    typeof value.ciphertext === "string" &&
    typeof value.createdAt === "string" &&
    typeof value.expiresAt === "string" &&
    includesString(ENCRYPTED_DEVICE_GRANT_STATES, value.state) &&
    isOptionalString(value.acceptedAt)
  );
}

function isRecoveryKeyState(value: unknown): value is RecoveryKeyState {
  return (
    isRecord(value) &&
    includesString(RECOVERY_KEY_LIFECYCLES, value.lifecycle) &&
    isOptionalString(value.envelopeId) &&
    isOptionalString(value.fingerprint) &&
    isOptionalString(value.createdAt) &&
    isOptionalString(value.verifiedAt) &&
    isOptionalString(value.rotatedAt) &&
    isOptionalString(value.revokedAt)
  );
}

function isBootstrapStep(value: unknown): value is BootstrapStep {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    includesString(BOOTSTRAP_STEP_STATES, value.state) &&
    typeof value.summary === "string"
  );
}

function isWorkView(value: unknown): value is WorkView {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.workspaceId === "string" &&
    typeof value.projectId === "string" &&
    typeof value.projectPath === "string" &&
    typeof value.name === "string" &&
    typeof value.visiblePath === "string" &&
    typeof value.baseSnapshotId === "string" &&
    typeof value.overlayHead === "string" &&
    typeof value.overlayVersion === "number" &&
    typeof value.envProfile === "string" &&
    includesString(WORK_VIEW_LIFECYCLES, value.lifecycle) &&
    includesString(WORK_VIEW_VISIBILITIES, value.visibility) &&
    includesString(WORK_VIEW_SYNC_STATES, value.syncState) &&
    isWorkViewRetention(value.retention) &&
    isOptionalString(value.ownerDeviceId) &&
    isStringArray(value.followedBy) &&
    isStringArray(value.hostMaterializations) &&
    isStringArray(value.attention) &&
    typeof value.createdAt === "string" &&
    typeof value.updatedAt === "string"
  );
}

function isWorkViewRetention(value: unknown): value is WorkViewRetention {
  return (
    isRecord(value) &&
    includesString(WORK_VIEW_RETENTION_STATES, value.state) &&
    isOptionalString(value.retainUntil) &&
    typeof value.restorable === "boolean"
  );
}

function isWorkDiffEntry(value: unknown): value is WorkDiffEntry {
  return (
    isRecord(value) &&
    typeof value.path === "string" &&
    includesString(WORK_DIFF_CHANGE_KINDS, value.kind) &&
    typeof value.summary === "string" &&
    typeof value.containsSecrets === "boolean"
  );
}

function isAccessFlags(value: unknown): value is readonly AccessFlag[] {
  return (
    Array.isArray(value) &&
    value.every((item) => includesString(ACCESS_FLAGS, item))
  );
}

function isNamespaceEntry(value: unknown): value is NamespaceEntry {
  return (
    isRecord(value) &&
    typeof value.path === "string" &&
    includesString(NAMESPACE_ENTRY_KINDS, value.kind) &&
    includesString(PATH_CLASSIFICATIONS, value.classification) &&
    includesString(MATERIALIZATION_MODES, value.mode) &&
    (value.access === undefined || isAccessFlags(value.access)) &&
    isOptionalString(value.contentId) &&
    (value.locator === undefined || isContentLocator(value.locator)) &&
    isOptionalString(value.symlinkTarget) &&
    isOptionalNonNegativeNumber(value.byteLen) &&
    includesString(HYDRATION_STATES, value.hydrationState)
  );
}

function isContentLocator(value: unknown): value is ContentLocator {
  return (
    isRecord(value) &&
    typeof value.contentId === "string" &&
    includesString(CONTENT_STORAGES, value.storage) &&
    isNonNegativeNumber(value.rawSize) &&
    isOptionalString(value.packId) &&
    isOptionalNonNegativeNumber(value.offset) &&
    isOptionalNonNegativeNumber(value.length) &&
    (value.chunkIds === undefined || isStringArray(value.chunkIds))
  );
}

function isWorkspaceRef(value: unknown): value is WorkspaceRef {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    typeof value.targetSnapshotId === "string" &&
    includesString(REF_KINDS, value.kind)
  );
}

function isLimitedCapabilities(
  value: unknown,
): value is readonly LimitedCapability[] {
  return Array.isArray(value) && value.every(isLimitedCapability);
}

function isLimitedCapability(value: unknown): value is LimitedCapability {
  return (
    isRecord(value) &&
    typeof value.capability === "string" &&
    typeof value.unavailableBecause === "string" &&
    isStringArray(value.stillWorks) &&
    isOptionalString(value.path)
  );
}

function isSyncQueueStatus(value: unknown): value is SyncQueueStatus {
  return (
    isRecord(value) &&
    isNonNegativeInteger(value.queued) &&
    isNonNegativeInteger(value.claimed) &&
    isNonNegativeInteger(value.waitingRetry) &&
    isNonNegativeInteger(value.blockedOffline) &&
    isNonNegativeInteger(value.attention) &&
    isNonNegativeInteger(value.completed)
  );
}

function isEventWatermarks(value: unknown): value is EventWatermarks {
  if (!isRecord(value)) return false;
  if (!isOptionalString(value.lastScanAt)) return false;
  if (!isOptionalString(value.lastEventId)) return false;
  if (
    value.eventLagMs !== undefined &&
    (typeof value.eventLagMs !== "number" || value.eventLagMs < 0)
  ) {
    return false;
  }
  if (
    value.syncState !== undefined &&
    !includesString(COMPONENT_STATES, value.syncState)
  ) {
    return false;
  }
  if (
    value.watcherState !== undefined &&
    !includesString(COMPONENT_STATES, value.watcherState)
  ) {
    return false;
  }
  if (
    value.networkState !== undefined &&
    !includesString(NETWORK_STATES, value.networkState)
  ) {
    return false;
  }

  return true;
}

function isNonNegativeNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function isNonNegativeInteger(value: unknown): value is number {
  return isNonNegativeNumber(value) && Number.isInteger(value);
}

function isOptionalNonNegativeNumber(
  value: unknown,
): value is number | undefined {
  return value === undefined || isNonNegativeNumber(value);
}

const COMPONENT_STATES = [
  "ready",
  "degraded",
  "unavailable",
] as const satisfies readonly ComponentState[];
const NETWORK_STATES = [
  "online",
  "degraded",
  "offline",
] as const satisfies readonly NetworkState[];

function isSafeActions(value: unknown): value is readonly SafeAction[] {
  return Array.isArray(value) && value.every(isSafeAction);
}

function isSafeAction(value: unknown): value is SafeAction {
  return (
    isRecord(value) &&
    typeof value.label === "string" &&
    isOptionalString(value.command) &&
    (value.effectCategory === undefined ||
      ["inspect", "trust", "setup", "mutate", "destructive"].includes(
        value.effectCategory as string,
      )) &&
    (value.targetKind === undefined ||
      [
        "workspace",
        "device",
        "setup",
        "work-view",
        "conflict",
        "agent",
        "recovery",
        "unknown",
      ].includes(value.targetKind as string))
  );
}

function isResolveConflict(value: unknown): value is ResolveConflict {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    value.state === "unresolved" &&
    typeof value.bundlePath === "string" &&
    isOptionalString(value.conflictKind) &&
    isStringArray(value.affectedFiles) &&
    (value.spans === undefined ||
      (Array.isArray(value.spans) &&
        value.spans.every(isResolveConflictSpan))) &&
    typeof value.activeView === "string" &&
    typeof value.hasResolutionOverlay === "boolean" &&
    typeof value.containsSecrets === "boolean"
  );
}

function isResolveConflictSpan(value: unknown): value is ResolveConflictSpan {
  return (
    isRecord(value) &&
    typeof value.path === "string" &&
    typeof value.baseStartLine === "number" &&
    typeof value.baseEndLine === "number" &&
    typeof value.localStartLine === "number" &&
    typeof value.localEndLine === "number" &&
    typeof value.remoteStartLine === "number" &&
    typeof value.remoteEndLine === "number" &&
    isOptionalString(value.baseContextHash) &&
    isOptionalString(value.localContextHash) &&
    isOptionalString(value.remoteContextHash)
  );
}

function isResolveAgentOption(value: unknown): value is ResolveAgentOption {
  return (
    isRecord(value) &&
    includesString(RESOLVE_AGENTS, value.name) &&
    typeof value.command === "string" &&
    isAgentCliCapability(value.capability)
  );
}

function isResolveAvailableActions(
  value: unknown,
): value is readonly ResolveAvailableAction[] {
  return Array.isArray(value) && value.every(isResolveAvailableAction);
}

function isResolveAvailableAction(
  value: unknown,
): value is ResolveAvailableAction {
  return (
    isRecord(value) &&
    typeof value.label === "string" &&
    isOptionalString(value.command)
  );
}

function isResolvePrompt(value: unknown): value is ResolvePrompt {
  return (
    isRecord(value) &&
    typeof value.conflictId === "string" &&
    typeof value.bundlePath === "string" &&
    typeof value.resolutionPath === "string" &&
    value.redaction === "applied" &&
    typeof value.text === "string"
  );
}

function isResolveDiff(value: unknown): value is ResolveDiff {
  return (
    isRecord(value) &&
    typeof value.conflictId === "string" &&
    typeof value.bundlePath === "string" &&
    value.redaction === "contents-not-printed" &&
    isStringArray(value.affectedFiles) &&
    typeof value.text === "string"
  );
}

function isResolveStatus(
  value: unknown,
): value is ResolveCommandOutput["status"] {
  return (
    isRecord(value) &&
    isStatusLevel(value.level) &&
    typeof value.summary === "string"
  );
}

function isOptionalString(value: unknown): value is string | undefined {
  return value === undefined || typeof value === "string";
}

function isOptionalCursor(value: unknown): value is string | undefined {
  return (
    value === undefined || (typeof value === "string" && /^v1:\d+$/.test(value))
  );
}

function isOptionalWorkspaceSummary(
  value: unknown,
): value is WorkspaceSummary | undefined {
  if (value === undefined) return true;
  if (!isRecord(value)) return false;
  if (
    value.projectsNeedingAttention !== undefined &&
    !(
      Array.isArray(value.projectsNeedingAttention) &&
      value.projectsNeedingAttention.every(isProjectAttentionSummary)
    )
  ) {
    return false;
  }
  if (
    value.totalProjects !== undefined &&
    (typeof value.totalProjects !== "number" || value.totalProjects < 0)
  ) {
    return false;
  }
  if (
    value.observed !== undefined &&
    !isObservedWorkspaceSummary(value.observed)
  ) {
    return false;
  }

  return true;
}

function isObservedWorkspaceSummary(
  value: unknown,
): value is ObservedWorkspaceSummary {
  if (!isRecord(value)) return false;
  return [
    value.repoCount,
    value.noRemoteRepoCount,
    value.staleRemoteTrackingRepoCount,
    value.generatedPathCount,
    value.dependencyPathCount,
    value.envFileCount,
    value.untrackedFileCount,
    value.localOnlyPathCount,
    value.blockedPathCount,
    value.workspaceSyncPathCount,
  ].every(isNonNegativeNumber);
}

function isProjectAttentionSummary(
  value: unknown,
): value is ProjectAttentionSummary {
  return (
    isRecord(value) &&
    typeof value.projectId === "string" &&
    typeof value.path === "string" &&
    isStatusLevel(value.level) &&
    typeof value.summary === "string"
  );
}
