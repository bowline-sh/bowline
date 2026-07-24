export type { BootstrapSshCommandOutput } from "./bootstrap";
export type {
  CommandErrorOutput,
  ContractCommandOutput,
  ContractSummaryCommandOutput,
  DaemonCommandOutput,
  DaemonServiceOutput,
  DaemonStatusOutput,
  DiagnosticsCollectCommandOutput,
  DoctorCommandOutput,
  DevicesCommandOutput,
  DryRunCommandOutput,
  HelpCommandOutput,
  HandoffCommandOutput,
  HandoffInstallReceipt,
  HistoryCommandOutput,
  LoginCommandOutput,
  LogoutCommandOutput,
  RecoveryCommandOutput,
  SetupCommandOutput,
  SetupProjectOutput,
  ScopedContractCommandOutput,
  StatusCommandOutput,
  UpdateCommandOutput,
  VersionCommandOutput,
  WatchFrame,
} from "./commands";
export type { EventName } from "./event-names";
export type { EventsCommandOutput, WorkspaceEvent } from "./events";
export type { ResolveCommandOutput } from "./resolve";
export type { ContentLayout, SnapshotManifest } from "./snapshot";
export type {
  DeviceApprovalAffordance,
  RepairCommand,
  StatusLevel,
  WorkspaceStatus,
} from "./status";
export type {
  WorkCleanupCommandOutput,
  WorkDiffCommandOutput,
  WorkLifecycleCommandOutput,
  WorkListCommandOutput,
  WorkCreateCommandOutput,
} from "./work";

import type { DeviceApprovalAffordance, RepairCommand } from "./status";

export type DeviceApprovalAffordances = readonly DeviceApprovalAffordance[];
export type RepairCommands = readonly RepairCommand[];
