import type { AccountLoginState } from "./account";
import type {
  DeviceApprovalRequest,
  DeviceRecord,
  EncryptedDeviceGrant,
  RecoveryKeyState,
  RevokedDevice,
} from "./devices";
import type { WorkspaceEvent } from "./events";
import type { StatusSummary } from "./generated/wire-contracts";
import type {
  CONTRACT_VERSION,
  DeviceId,
  EventId,
  ProjectId,
  SnapshotId,
  WorkspaceId,
} from "./ids";
import type {
  DeviceApprovalAffordance,
  EventWatermarks,
  FreshnessVerdict,
  LimitedCapability,
  ProjectSetupReadiness,
  RepairCommand,
  StaleBaseStatus,
  StatusItem,
  StatusScope,
  SyncQueueStatus,
  WorkspaceStatus,
  WorkspaceSummary,
} from "./status";
import type { CommandName } from "./command-names";

export { COMMAND_NAMES, type CommandName } from "./command-names";

type CommandErrorName = CommandName;

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

export type CliCommandPositional = {
  readonly name: string;
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
  readonly summary: string;
  readonly usage: string;
  readonly positionals: readonly CliCommandPositional[];
  readonly options?: readonly CliCommandOption[];
  readonly examples?: readonly CliCommandExample[];
  readonly jsonOutputType: string;
  readonly sideEffectLevel: string;
  readonly supportsJson: boolean;
  readonly supportsDryRun: boolean;
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

export type UpdateCommandOutput = CommandOutputBase<"update"> & {
  readonly ok: boolean;
  readonly currentVersion: string;
  readonly latestVersion: string;
  readonly updateAvailable: boolean;
  readonly updateCommand: string;
};

export type ContractFixtureDescriptor = {
  readonly name: string;
  readonly path: string;
  readonly outputType: string;
};

export type ContractExitCodes = {
  readonly success: 0;
  readonly usageError: 2;
  readonly retryableRuntimeError: 3;
  readonly userActionRequired: 4;
  readonly blockedOrDegradedBySafety: 5;
};

export type ContractCommandEnvelope = CommandOutputBase<"contract"> & {
  readonly cliVersion: string;
  readonly protocol: string;
  readonly protocolVersion: number;
  readonly eventSchemaVersion: number;
  readonly package: string;
  readonly packageContractSource: string;
  readonly exitCodes: ContractExitCodes;
};

export type ContractCommandOutput = ContractCommandEnvelope & {
  readonly commandOutputTypes: readonly string[];
  readonly commands: readonly CliCommandDescriptor[];
  readonly fixtures: readonly ContractFixtureDescriptor[];
};

export type ScopedContractCommandOutput = ContractCommandEnvelope & {
  readonly descriptor: CliCommandDescriptor;
};

export type CliCommandSummary = Pick<
  CliCommandDescriptor,
  | "name"
  | "group"
  | "summary"
  | "sideEffectLevel"
  | "supportsJson"
  | "supportsDryRun"
>;

export type ContractSummaryCommandOutput = ContractCommandEnvelope & {
  readonly commands: readonly CliCommandSummary[];
};

export const HANDOFF_AGENTS = ["codex", "claude"] as const;
export type HandoffAgent = (typeof HANDOFF_AGENTS)[number];
export const HANDOFF_OUTCOMES = [
  "dry_run",
  "confirmation_required",
  "receipt",
  "error",
] as const;
export type HandoffOutcome = (typeof HANDOFF_OUTCOMES)[number];
export const HANDOFF_SESSION_MODES = [
  "resume_existing",
  "fresh_prompt",
] as const;
export type HandoffSessionMode = (typeof HANDOFF_SESSION_MODES)[number];

export type HandoffCandidate = {
  readonly agent: HandoffAgent;
  readonly sessionId: string;
  readonly sourcePath: string;
  readonly projectPath?: string;
  readonly modifiedAtUnixSeconds: number;
  readonly selected: boolean;
  readonly skippedReason?: string;
};

export type HandoffTransferPlan = {
  readonly encrypted: boolean;
  readonly durableCloudStorage: boolean;
  readonly installsByteExactSessionFiles: boolean;
  readonly remoteInstallerCommand: string;
};

export type HandoffPlan = {
  readonly target: string;
  readonly agent: HandoffAgent;
  readonly sessionMode: HandoffSessionMode;
  readonly projectPath: string;
  readonly remoteProjectPath: string;
  readonly tmuxSession: string;
  readonly launchCommand: string;
  readonly transfer: HandoffTransferPlan;
};

export type HandoffReceipt = {
  readonly agent: HandoffAgent;
  readonly target: string;
  readonly remoteProjectPath: string;
  readonly tmuxSession: string;
  readonly attachCommand: string;
  readonly monitoring: boolean;
  readonly workspaceLock: boolean;
  readonly sameSessionConcurrencyRisk: boolean;
  readonly sessionMode: HandoffSessionMode;
  readonly agentRuntimeVerified: boolean;
  readonly note: string;
};

export type HandoffInstallReceipt = {
  readonly agent: HandoffAgent;
  readonly sessionMode: HandoffSessionMode;
  readonly sessionId?: string;
  readonly installedFiles: readonly string[];
  readonly promptFile?: string;
  readonly remoteProjectPath: string;
};

export type HandoffError = {
  readonly code: string;
  readonly message: string;
  readonly recoverability: string;
};

export type HandoffCommandOutput = CommandOutputBase<"handoff"> & {
  readonly outcome: HandoffOutcome;
  readonly target: string;
  readonly projectPath: string;
  readonly candidates: readonly HandoffCandidate[];
  readonly selected?: HandoffCandidate;
  readonly plan?: HandoffPlan;
  readonly receipt?: HandoffReceipt;
  readonly error?: HandoffError;
  readonly nextActions: readonly RepairCommand[];
};

export type DryRunCommandOutput = CommandOutputBase<CommandName> & {
  readonly status: "dry-run";
  readonly allowed: boolean;
  readonly risk: string;
  readonly target: string;
  readonly wouldChange: readonly string[];
  readonly warnings?: readonly string[];
  readonly applyCommand: string;
  readonly nextActions: readonly RepairCommand[];
};

export type NamespaceLifecycleAction =
  | "forget-local"
  | "archive"
  | "restore"
  | "purge-pending"
  | "purge-cancel";

export type NamespaceLifecyclePreview = {
  readonly paths: readonly string[];
  readonly byteTotal: number;
  readonly packCount: number;
  readonly graceDays?: number;
  readonly purgeAfter?: string;
};

export type NamespaceLifecycleCommandOutput = CommandOutputBase<
  "forget-local" | "archive" | "purge"
> & {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly projectPath: string;
  readonly action: NamespaceLifecycleAction;
  readonly preview: NamespaceLifecyclePreview;
  readonly changed: boolean;
  readonly nextActions: readonly RepairCommand[];
};

export type DaemonProcessOutput = {
  readonly state: string;
  readonly socket: string;
  readonly syncState?: DaemonSyncState;
  readonly unavailableBecause?: string;
  readonly protocol?: string;
  readonly version?: number;
  readonly daemonVersion?: string;
  readonly pid?: number;
};

export const DAEMON_SYNC_STATES = [
  "limited",
  "degraded",
  "unclassified",
] as const;
export type DaemonSyncState = (typeof DAEMON_SYNC_STATES)[number];

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

export type DoctorEngine = "manifest";

export type DoctorCheckStatus = "ok" | "degraded" | "unavailable" | "failed";

export type DoctorCheckId =
  | "engine-sqlite-integrity"
  | "ancestor-ref-consistency"
  | "intent-recoverability"
  | "watcher-health"
  | "ref-fetch-verification"
  | "ref-metadata-object-existence"
  | "sealed-content-id-verification"
  | "workspace-key-availability"
  | "retry-age"
  | "portable-path-collisions"
  | "temp-capacity"
  | "atomic-rename-capability"
  | "deployment-identity"
  | "installed-candidate-hash";

// Fixed, safe reason codes. The Rust `DoctorReason` enum is the authority; this
// union mirrors it for the TypeScript fixture decoder (the codebase's standard
// cross-language command-output contract mechanism).
export type DoctorReason =
  | "integrity-verified"
  | "integrity-failed"
  | "engine-database-missing"
  | "ancestor-consistent"
  | "ancestor-missing"
  | "ref-regressed-below-verified"
  | "intents-recoverable"
  | "intent-unclassifiable"
  | "watcher-healthy"
  | "watcher-recovery-pending"
  | "daemon-unreachable"
  | "ref-verified"
  | "ref-signature-unverifiable"
  | "ref-absent"
  | "control-plane-unreachable"
  | "object-present"
  | "metadata-missing"
  | "object-missing"
  | "sample-verified"
  | "content-id-mismatch"
  | "seal-verification-unavailable"
  | "sample-empty"
  | "key-available"
  | "key-unavailable"
  | "epoch-mismatch"
  | "retry-nominal"
  | "retry-stale"
  | "no-collisions"
  | "portable-path-collision"
  | "capacity-sufficient"
  | "capacity-insufficient"
  | "state-root-unavailable"
  | "rename-supported"
  | "rename-unsupported"
  | "identity-matched"
  | "identity-mismatched"
  | "identity-unknown"
  | "hash-computed"
  | "hash-unavailable";

export type DoctorCheck = {
  readonly id: DoctorCheckId;
  readonly status: DoctorCheckStatus;
  readonly reason: DoctorReason;
  readonly count?: number;
  readonly opaque?: string;
};

export type DoctorSummary = {
  readonly ok: number;
  readonly degraded: number;
  readonly unavailable: number;
  readonly failed: number;
  readonly attentionRequired: boolean;
};

export type DoctorCommandOutput = CommandOutputBase<"doctor"> & {
  readonly workspaceId: WorkspaceId;
  readonly engine: DoctorEngine;
  readonly summary: DoctorSummary;
  readonly checks: readonly DoctorCheck[];
};

export type LoginCommandOutput = CommandOutputBase<"login"> & {
  readonly account: AccountLoginState;
  readonly localDevice?: DeviceRecord;
  readonly nextActions: readonly RepairCommand[];
};

export type LogoutCommandOutput = CommandOutputBase<"logout"> & {
  readonly signedOut: boolean;
  readonly nextActions: readonly RepairCommand[];
};

export type StatusCommandOutput = CommandOutputBase<"status"> & {
  readonly scope?: StatusScope;
  readonly requestedPath?: string;
  readonly resolvedWorkspaceRoot?: string;
  readonly resolvedProjectRoot?: string;
  readonly workspaceSummary?: WorkspaceSummary;
  readonly setupReadiness?: ProjectSetupReadiness;
  readonly syncQueue?: SyncQueueStatus;
  readonly freshness: FreshnessVerdict;
  readonly staleBases?: readonly StaleBaseStatus[];
  readonly status: WorkspaceStatus;
  readonly statusSummary: StatusSummary;
  readonly items: readonly StatusItem[];
  readonly limits: readonly LimitedCapability[];
  readonly eventWatermarks: EventWatermarks;
  readonly nextActions: readonly RepairCommand[];
  // Sensitive local trust material; present only on trusted local status
  // surfaces and omitted from hosted/persisted payloads.
  readonly deviceApprovals?: readonly DeviceApprovalAffordance[];
};

export type RootChoiceState =
  | "explicit-existing"
  | "explicit-created"
  | "default-selected"
  | "ambiguous";

export type SetupProjectState = "hot" | "setup-blocked" | "no-setup-needed";

export type SetupProjectOutcome = {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly projectPath: string;
  readonly state: SetupProjectState;
  readonly receiptIds: readonly string[];
  readonly redactedSummary: string;
};

export type SetupProjectOutput = CommandOutputBase<"setup"> & {
  readonly outcome: SetupProjectOutcome;
};

export type SetupCommandOutput = CommandOutputBase<"setup"> & {
  readonly workspaceId: WorkspaceId;
  readonly root: string;
  readonly rootChoice: RootChoiceState;
  readonly login: AccountLoginState;
  readonly nextActions: readonly RepairCommand[];
  readonly connectedHost?: string;
};

export type HistoryScopeKind = "project" | "path";

export type HistoryScope = {
  readonly kind: HistoryScopeKind;
  readonly root: string;
  readonly projectPath: string;
  readonly projectId: ProjectId;
  readonly path?: string;
};

export type HistoryCause =
  | "sync"
  | "accept"
  | "conflict-resolution"
  | "restore"
  | "lifecycle"
  | "unknown";

export type HistoryActorKind = "human" | "agent" | "daemon" | "control-plane";

export type HistoryActor = {
  readonly kind: HistoryActorKind;
  readonly displayName?: string;
  readonly deviceId?: DeviceId;
};

export type HistoryChangeSummary = {
  readonly filesChanged: number;
  readonly filesAdded: number;
  readonly filesModified: number;
  readonly filesDeleted: number;
  readonly filesRenamed: number;
  readonly binaryOrLargeFilesChanged: number;
  readonly envKeysChanged: number;
  readonly pathsSample: readonly string[];
};

export type RestorePoint = {
  readonly id: string;
  readonly snapshotId: SnapshotId;
  readonly baseSnapshotId?: SnapshotId;
  readonly occurredAt: string;
  readonly label: string;
  readonly cause: HistoryCause;
  readonly actor?: HistoryActor;
  readonly summary: HistoryChangeSummary;
  readonly eventIds: readonly EventId[];
};

export type PathHistoryOperation =
  | "create"
  | "modify"
  | "delete"
  | "rename"
  | "policy"
  | "unknown";

export type PathHistoryEntry = {
  readonly restorePointId: string;
  readonly snapshotId: SnapshotId;
  readonly occurredAt: string;
  readonly operation: PathHistoryOperation;
  readonly sourcePath?: string;
  readonly actor?: HistoryActor;
  readonly causationId?: EventId;
  readonly eventIds: readonly EventId[];
};

export type HistoryEndpoint = {
  readonly restorePointId?: string;
  readonly snapshotId: SnapshotId;
};

export type HistoryCommandOutput = CommandOutputBase<"history"> & {
  readonly workspaceId: WorkspaceId;
  readonly projectId: ProjectId;
  readonly scope: HistoryScope;
  readonly restorePoints: readonly RestorePoint[];
  readonly pathEntries: readonly PathHistoryEntry[];
  readonly from?: HistoryEndpoint;
  readonly to?: HistoryEndpoint;
  readonly diffSummary?: HistoryChangeSummary;
  readonly nextCursor?: string;
  readonly truncated: boolean;
  readonly nextActions: readonly RepairCommand[];
};

export type DevicesCommandOutput = CommandOutputBase<
  | "device approve"
  | "device deny"
  | "device revoke"
  | "device list"
  | "device request"
  | "device accept"
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
  readonly nextActions: readonly RepairCommand[];
};

export type RecoveryCommandOutput = CommandOutputBase<"recover"> & {
  readonly action: "status" | "create" | "verify" | "rotate" | "revoke" | "use";
  readonly recoveryKey: RecoveryKeyState;
  readonly deviceRequest?: DeviceApprovalRequest;
  readonly encryptedGrant?: EncryptedDeviceGrant;
  readonly nextActions: readonly RepairCommand[];
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
  readonly nextActions?: readonly RepairCommand[];
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
