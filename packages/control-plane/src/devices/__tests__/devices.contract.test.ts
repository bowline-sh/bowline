import { describe, expect, it } from "vitest";

import { approveDevice } from "../index";

describe("device approval contract", () => {
  it("turns an approval into a workspace-wide authorized device", () => {
    const device = approveDevice({
      authorizedAt: "2026-06-23T12:00:00Z",
      deviceFingerprint: "fp_linux" as never,
      deviceId: "device_linux" as never,
      deviceName: "linux-server-1",
      platform: "linux",
      workspaceId: "workspace_code" as never,
    });

    expect(device).toEqual({
      authorizedAt: "2026-06-23T12:00:00Z",
      deviceFingerprint: "fp_linux",
      id: "device_linux",
      name: "linux-server-1",
      platform: "linux",
      workspaceId: "workspace_code",
    });
  });
});
