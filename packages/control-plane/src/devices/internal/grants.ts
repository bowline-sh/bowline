import type { AuthorizedDevice } from "@bowline/contracts";

import type { DeviceApprovalInput } from "../types";

export function buildAuthorizedDevice(
  input: DeviceApprovalInput,
): AuthorizedDevice {
  return {
    authorizedAt: input.authorizedAt,
    ...(input.authorizedByDeviceId === undefined
      ? {}
      : { authorizedByDeviceId: input.authorizedByDeviceId }),
    deviceFingerprint: input.deviceFingerprint,
    id: input.deviceId,
    name: input.deviceName,
    platform: input.platform,
    workspaceId: input.workspaceId,
  };
}
