import type { EventName } from "./event-names";
import { isEventName, isStatusLevel } from "./generated-guards";
import type { StatusLevel, WorkspaceStatus } from "./status";

export {
  isBootstrapSshCommandOutput,
  isCommandErrorOutput,
  isContentLayout,
  isContractCommandOutput,
  isContractSummaryCommandOutput,
  isDaemonCommandOutput,
  isDaemonServiceOutput,
  isDaemonStatusOutput,
  isDeviceApprovalAffordance,
  isDeviceApprovalAffordances,
  isDevicesCommandOutput,
  isDiagnosticsCollectCommandOutput,
  isDoctorCommandOutput,
  isDryRunCommandOutput,
  isEventName,
  isEventsCommandOutput,
  isHandoffCommandOutput,
  isHandoffInstallReceipt,
  isHelpCommandOutput,
  isHistoryCommandOutput,
  isLoginCommandOutput,
  isLogoutCommandOutput,
  isRecoveryCommandOutput,
  isRepairCommand,
  isRepairCommands,
  isResolveCommandOutput,
  isSetupCommandOutput,
  isSetupProjectOutput,
  isScopedContractCommandOutput,
  isSnapshotManifest,
  isStatusCommandOutput,
  isStatusLevel,
  isUpdateCommandOutput,
  isVersionCommandOutput,
  isWatchFrame,
  isWorkCreateCommandOutput,
  isWorkCleanupCommandOutput,
  isWorkDiffCommandOutput,
  isWorkLifecycleCommandOutput,
  isWorkListCommandOutput,
  isWorkspaceEvent,
  isWorkspaceStatus,
} from "./generated-guards";

export function parseEventName(value: unknown): EventName {
  if (isEventName(value)) return value;
  throw new Error(`Unknown event name: ${String(value)}`);
}

export function parseStatusLevel(value: unknown): StatusLevel {
  if (isStatusLevel(value)) return value;
  throw new Error(`Unknown status level: ${String(value)}`);
}

export function statusNeedsAttention(status: WorkspaceStatus): boolean {
  return status.level !== "healthy" || status.attentionItems.length > 0;
}
