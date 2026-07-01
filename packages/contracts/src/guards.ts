import type { AccountLoginState, AccountLoginStatus } from "./account";
import { AGENT_LEASE_CLEANUP_STATES, AGENT_LEASE_EXECUTION_STATES, AGENT_LEASE_OUTPUT_STATES, AGENT_TOOL_NAMES, type AgentAuditPointer, type AgentBudgetCommandOutput, type AgentCapability, type AgentCliCapability, type AgentCliName, type AgentContextCommandOutput, type AgentContextV1, type AgentEnvProfile, type AgentEnvRestriction, type AgentLease, type AgentLeaseCreateCommandOutput, type AgentLeaseScope, type AgentLeaseScopes, type AgentOutputTarget, type AgentProjectReadiness, type AgentPrompt, type AgentPromptCommandOutput, type AgentReadinessSignal, type AgentReadinessState, type AgentStartWork, type AgentToolDenial, type AgentToolResult, type DegradedExplorationBounds } from "./agent";
import { type BootstrapSshCommandOutput, type BootstrapStep, type BootstrapStepState } from "./bootstrap";
import { CONTRACT_VERSION } from "./ids";
import { COMMAND_NAMES, type ActionsCommandOutput, type BoundedOutputControls, type CliCommandDescriptor, type CliCommandExample, type CliCommandGroup, type CliCommandOption, type CommandError, type CommandErrorOutput, type CommandErrorStatus, type CommandRecoverability, type ContractCommandOutput, type ContractFixtureDescriptor, type DaemonCommandOutput, type DaemonProcessOutput, type DaemonServiceOutput, type DaemonServiceState, type DaemonStatusOutput, type DevicesCommandOutput, type DiagnosticsCollectCommandOutput, type DryRunCommandOutput, type ExplainCommandOutput, type HelpCommandOutput, type InitCommandOutput, type LoginCommandOutput, type PrewarmCommandOutcome, type PrewarmCommandOutput, type PrewarmCommandState, type RecoveryCommandOutput, type RootChoiceState, type StatusCommandOutput, type VersionCommandOutput, type WatchFrame } from "./commands";
import { DEVICE_APPROVAL_REQUEST_STATES, type DeviceApprovalRequest, type DevicePlatform, type DeviceRecord, type DeviceTrustState, type EncryptedDeviceGrant, type EncryptedDeviceGrantState, type RecoveryKeyLifecycle, type RecoveryKeyState, type RevokedDevice } from "./devices";
import { EVENT_NAMES, type EventName } from "./event-names";
import type { EventActor, EventActorKind, EventRedaction, EventSeverity, EventSubject, EventSubjectKind, EventsCommandOutput, WorkspaceEvent } from "./events";
import { ACCESS_FLAGS, MATERIALIZATION_MODES, PATH_CLASSIFICATIONS, type AccessFlag } from "./policy";
import type { ResolveAction, ResolveAgent, ResolveAgentOption, ResolveAvailableAction, ResolveCommandOutput, ResolveConflict, ResolveConflictSpan, ResolveDiff, ResolvePrompt } from "./resolve";
import { SYMBOL_KINDS, SYMBOL_LANGUAGES, type SearchCommandOutput, type SearchResult, type SymbolCommandOutput, type SymbolResult } from "./search";
import { CONTENT_STORAGES, HYDRATION_STATES, NAMESPACE_ENTRY_KINDS, REF_KINDS, SNAPSHOT_KINDS, type ContentLocator, type NamespaceEntry, type SnapshotManifest, type WorkspaceRef } from "./snapshot";
import { HYDRATION_BUDGET_STATES, INDEX_STATES, STATUS_LEVELS, STATUS_SCOPES, type ComponentState, type EventWatermarks, type HydrationBudgetStatus, type HydrationProgress, type IndexDegradedReason, type IndexStatus, type LimitedCapability, type NetworkState, type ObservedWorkspaceSummary, type ProjectAttentionSummary, type SafeAction, type StatusItem, type StatusItemKind, type StatusLevel, type StatusScope, type StatusSubject, type StatusSubjectKind, type SyncQueueStatus, type WorkspaceStatus, type WorkspaceSummary } from "./status";
import { WORK_DIFF_CHANGE_KINDS, WORK_VIEW_LIFECYCLES, WORK_VIEW_RETENTION_STATES, WORK_VIEW_SYNC_STATES, WORK_VIEW_VISIBILITIES, type WorkCleanupCommandOutput, type WorkDiffCommandOutput, type WorkDiffEntry, type WorkLifecycleCommandOutput, type WorkListCommandOutput, type WorkView, type WorkViewRetention, type WorkonCommandOutput } from "./work";

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
    value.command === "recover" &&
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
    value.command === "connect" &&
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
    value.command === "agent start" &&
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
    includesString(COMMAND_NAMES, value.command) &&
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
