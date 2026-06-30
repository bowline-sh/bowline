import type {
  AuthorizedDevice,
  DeviceFingerprint,
  DeviceId,
  DevicePlatform,
  WorkspaceId,
} from "@bowline/contracts";

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
