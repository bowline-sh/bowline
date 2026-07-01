import type {
  AuthorizedDevice,
  DeviceFingerprint,
  DevicePlatform,
} from "@bowline/contracts/devices";
import type { DeviceId, WorkspaceId } from "@bowline/contracts/ids";

export type DeviceApprovalInput = {
  readonly authorizedAt: string;
  readonly authorizedByDeviceId?: DeviceId;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly deviceId: DeviceId;
  readonly deviceName: string;
  readonly platform: DevicePlatform;
  readonly workspaceId: WorkspaceId;
};

export type DeviceApprovalResult = AuthorizedDevice;
