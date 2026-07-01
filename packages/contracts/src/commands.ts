import type { AccountLoginState } from "./account";
import type { DeviceApprovalRequest, DeviceRecord, EncryptedDeviceGrant, RecoveryKeyState, RevokedDevice } from "./devices";
import type { WorkspaceEvent } from "./events";
import type { CONTRACT_VERSION, EventId, ProjectId, WorkspaceId } from "./ids";
import type { AccessFlag, MaterializationMode, PathClassification } from "./policy";
import type {
  EventWatermarks,
  HydrationBudgetStatus,
  HydrationProgress,
  IndexStatus,
  LimitedCapability,
  ObservedWorkspaceSummary,
  SafeAction,
  StatusItem,
  StatusScope,
  SyncQueueStatus,
  WorkspaceStatus,
  WorkspaceSummary,
} from "./status";

export const COMMAND_NAMES = [
  "help",
  "version",
  "contract",
  "unknown",
  "login",
  "logout",
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
  "connect",
] as const;
export type CommandName = (typeof COMMAND_NAMES)[number];

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

export type LogoutCommandOutput = CommandOutputBase<"logout"> & {
  readonly signedOut: boolean;
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

export type RecoveryCommandOutput = CommandOutputBase<"recover"> & {
  readonly action: "status" | "create" | "verify" | "rotate" | "revoke" | "use";
  readonly recoveryKey: RecoveryKeyState;
  readonly deviceRequest?: DeviceApprovalRequest;
  readonly encryptedGrant?: EncryptedDeviceGrant;
  readonly nextActions: readonly SafeAction[];
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
