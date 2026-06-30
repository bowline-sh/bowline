import type { AuthorizedDevice } from "@bowline/contracts";

import { buildAuthorizedDevice } from "./internal/grants";
import type { DeviceApprovalInput } from "./types";

export type { DeviceApprovalInput } from "./types";

export function approveDevice(input: DeviceApprovalInput): AuthorizedDevice {
  return buildAuthorizedDevice(input);
}
