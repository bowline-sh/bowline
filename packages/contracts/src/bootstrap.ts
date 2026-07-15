import type {
  DeviceApprovalRequest,
  DeviceFingerprint,
  DeviceRecord,
} from "./devices";
import type { RepairCommand, WorkspaceStatus } from "./status";
import type { CommandOutputBase } from "./commands";

export type BootstrapStepState = "pending" | "completed" | "blocked";

export type BootstrapStep = {
  readonly name: string;
  readonly state: BootstrapStepState;
  readonly summary: string;
};

export type BootstrapSshCommandOutput = CommandOutputBase<"connect"> & {
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
  readonly repairActions: readonly RepairCommand[];
};
