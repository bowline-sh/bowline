import { describe, expect, it } from "vitest";

import {
  ACCESS_FLAGS,
  CONTROL_PLANE_SUPPORT_CAPABILITIES,
  EVENT_NAMES,
  isEventName,
  isDeviceApprovalAffordance,
  isRepairCommand,
  isStatusCommandOutput,
  MATERIALIZATION_MODES,
  parseEventName,
  parseStatusLevel,
  PATH_CLASSIFICATIONS,
  SCHEMA_SOURCE_OF_TRUTH,
  STATUS_LEVELS,
  statusNeedsAttention,
  type WorkspaceStatus,
} from "../index";
import {
  isRecord,
  manifestEntriesFor,
  readContractFixture,
} from "./support/contractFixtures";

describe("workspace status contract", () => {
  it("treats healthy status without attention items as quiet", () => {
    const status: WorkspaceStatus = { attentionItems: [], level: "healthy" };

    expect(statusNeedsAttention(status)).toBe(false);
  });

  it("treats limited status as attention even without items", () => {
    const status: WorkspaceStatus = { attentionItems: [], level: "limited" };

    expect(statusNeedsAttention(status)).toBe(true);
  });

  it("defines the Phase 0B vocabulary from the implementation plan", () => {
    expect(STATUS_LEVELS).toEqual(["healthy", "attention", "limited"]);
    expect(PATH_CLASSIFICATIONS).toEqual([
      "workspace-sync",
      "project-env",
      "generated",
      "dependency",
      "cache",
      "large-file",
      "secret-looking",
      "local-only",
      "blocked",
    ]);
    expect(MATERIALIZATION_MODES).toEqual([
      "workspace-sync",
      "project-env",
      "encrypted-sync",
      "lazy",
      "structure-only",
      "local-regenerate",
      "local-cache",
      "ignore",
      "local-only",
      "blocked",
    ]);
    expect(ACCESS_FLAGS).toEqual([
      "human-readable",
      "agent-readable",
      "agent-hidden",
      "lease-only",
    ]);
    expect(EVENT_NAMES).toEqual([
      "namespace.created",
      "namespace.moved",
      "namespace.deleted_or_archived",
      "hydration.started",
      "hydration.completed",
      "hydration.blocked",
      "policy.classified",
      "policy.needs_approval",
      "policy.changed",
      "env.imported",
      "env.materialized",
      "env.revoked",
      "setup.started",
      "setup.completed",
      "setup.blocked",
      "source.stale",
      "work.created",
      "work.updated",
      "work.review_ready",
      "work.accepted",
      "work.discarded",
      "work.restored",
      "work.cleanup_previewed",
      "work.cleanup_completed",
      "lease.created",
      "lease.updated",
      "lease.dispatched",
      "lease.claimed",
      "lease.completed",
      "lease.cancelled",
      "lease.extended",
      "lease.review_ready",
      "overlay.changed",
      "conflict.created",
      "conflict.bundle_created",
      "conflict.resolution_proposed",
      "conflict.resolution_accepted",
      "conflict.resolution_rejected",
      "merge.plugin_applied",
      "daemon.degraded",
      "daemon.recovered",
      "device.approval_requested",
      "device.approved",
      "device.denied",
      "device.revoked",
      "recovery_key.created",
      "recovery_key.verified",
      "recovery_key.rotated",
      "recovery_key.revoked",
      "auth.login_started",
      "auth.login_completed",
      "sync.started",
      "sync.completed",
      "sync.limited",
      "sync.degraded",
      "sync.stat_cache_divergence",
      "sync.recovered",
      "watcher.degraded",
      "watcher.recovered",
      "network.offline",
      "network.recovered",
      "metadata.corrupt",
    ]);
    expect(SCHEMA_SOURCE_OF_TRUTH).toBe("contracts/wire");
    expect(CONTROL_PLANE_SUPPORT_CAPABILITIES).toEqual([
      "device-approval",
      "project-scoped-workspace-ref-cas",
      "work-view",
      "agent-lease",
      "encrypted-object-store",
      "recovery",
    ]);
  });

  it.each(statusFixtureNames())(
    "accepts the shared %s status fixture",
    (fixtureName) => {
      const fixture = readStatusFixture(fixtureName);

      expect(isStatusCommandOutput(fixture)).toBe(true);
      if (!isStatusCommandOutput(fixture)) return;

      expect(fixture.command).toBe("status");
      expect(statusNeedsAttention(fixture.status)).toBe(
        fixture.status.level !== "healthy" ||
          fixture.status.attentionItems.length > 0,
      );
      expect(
        fixture.items
          .map((item) => item.eventName)
          .filter((eventName) => eventName !== undefined)
          .every(isEventName),
      ).toBe(true);
    },
  );

  it("rejects unknown status levels and preserves unknown event names", () => {
    expect(() => parseStatusLevel("blocked")).toThrow(
      "Unknown status level: blocked",
    );
    expect(parseEventName("status.unknown")).toBe("status.unknown");
  });

  it("accepts concrete repair commands with a producer-set mutation flag", () => {
    expect(
      isRepairCommand({
        label: "Resolve conflicts",
        command: "bowline resolve ~/Code",
        mutates: true,
      }),
    ).toBe(true);

    expect(
      isRepairCommand({
        label: "Review path policy",
        mutates: false,
      }),
    ).toBe(true);
  });

  it("rejects repair commands missing the mutation flag", () => {
    expect(
      isRepairCommand({
        label: "Resolve conflicts",
        command: "bowline resolve ~/Code",
      }),
    ).toBe(false);

    expect(
      isRepairCommand({
        command: "bowline resolve ~/Code",
        mutates: true,
      }),
    ).toBe(false);
  });

  it("accepts device-approval affordances and rejects malformed ones", () => {
    expect(
      isDeviceApprovalAffordance({
        requestId: "device-request:ws_code:dev-mac",
        deviceName: "Dev Mac",
        code: "42-31",
        approveCommand: "bowline device approve --root ~/Code --code 42-31",
      }),
    ).toBe(true);

    expect(
      isDeviceApprovalAffordance({
        requestId: "device-request:ws_code:dev-mac",
        deviceName: "Dev Mac",
        approveCommand: "bowline device approve --root ~/Code --code 42-31",
      }),
    ).toBe(false);
  });

  it("applies device-approval refinements inside status output", () => {
    const approval = {
      requestId: "device-request:ws_code:dev-mac",
      deviceName: "Dev Mac",
      code: "42-31",
      approveCommand: "bowline device approve --root ~/Code --code 42-31",
    };
    const withApprovals = {
      ...expectRecord(readStatusFixture("healthy")),
      deviceApprovals: [approval, { ...approval, requestId: "second" }],
    };

    expect(isStatusCommandOutput(withApprovals)).toBe(true);
    expect(
      isStatusCommandOutput({
        ...withApprovals,
        deviceApprovals: [approval, { ...approval, requestId: "" }],
      }),
    ).toBe(false);
    expect(
      isStatusCommandOutput({
        ...withApprovals,
        deviceApprovals: [approval, { ...approval, approveCommand: "" }],
      }),
    ).toBe(false);
    expect(isDeviceApprovalAffordance({ ...approval, requestId: "" })).toBe(
      false,
    );
    const withoutDeviceName = { ...approval } as Partial<typeof approval>;
    delete withoutDeviceName.deviceName;
    expect(isDeviceApprovalAffordance(withoutDeviceName)).toBe(false);
    expect(isDeviceApprovalAffordance({ ...approval, deviceName: "" })).toBe(
      false,
    );
    expect(
      isDeviceApprovalAffordance({ ...approval, approveCommand: "" }),
    ).toBe(false);
    expect(
      isStatusCommandOutput({
        ...withApprovals,
        deviceApprovals: [{ ...approval, code: 42 }],
      }),
    ).toBe(false);
    expect(isStatusCommandOutput(readStatusFixture("healthy"))).toBe(true);
  });

  it("rejects unknown typed support capabilities", () => {
    const fixture = {
      ...expectRecord(readStatusFixture("healthy")),
      limits: [
        {
          capability: "Control plane support",
          supportCapability: "teleport-workspace",
          unavailableBecause: "The hosted control plane does not support it.",
          stillWorks: ["Local status"],
        },
      ],
    };

    expect(isStatusCommandOutput(fixture)).toBe(false);
  });

  it("rejects status output without workspaceId", () => {
    const withoutWorkspaceId = {
      ...expectRecord(readStatusFixture("healthy")),
    };
    delete withoutWorkspaceId.workspaceId;

    expect(isStatusCommandOutput(withoutWorkspaceId)).toBe(false);
  });

  it("requires sync queue counters to be nonnegative integers", () => {
    const negative = expectRecord(readStatusFixture("sync-queue-limited"));
    expectRecord(negative.syncQueue).queued = -1;
    expect(isStatusCommandOutput(negative)).toBe(false);

    const fractional = expectRecord(readStatusFixture("sync-queue-limited"));
    expectRecord(fractional.syncQueue).queued = 1.5;
    expect(isStatusCommandOutput(fractional)).toBe(false);
  });

  it("rejects unknown setup readiness states", () => {
    const invalid = {
      ...expectRecord(readStatusFixture("setup-readiness-blocked")),
      setupReadiness: {
        ...expectRecord(
          expectRecord(readStatusFixture("setup-readiness-blocked"))
            .setupReadiness,
        ),
        state: "ready-ish",
      },
    };

    expect(isStatusCommandOutput(invalid)).toBe(false);
  });

  it("accepts needs-setup setup readiness with a concrete remedy", () => {
    const fixture = {
      ...expectRecord(readStatusFixture("setup-readiness-blocked")),
      setupReadiness: {
        state: "needs-setup",
        reason: "Lockfile-backed setup has not run for pnpm-lock.yaml.",
        remedy: "Run setup for this hot project on the current machine.",
        identityHash: "setupid_needs",
      },
      nextActions: [
        {
          label: "Run setup",
          command: "bowline setup apps/web",
          mutates: true,
        },
      ],
    };

    expect(isStatusCommandOutput(fixture)).toBe(true);
  });

  it("accepts runnable setup readiness without receipt metadata", () => {
    const fixture = {
      ...expectRecord(readStatusFixture("setup-readiness-blocked")),
      setupReadiness: {
        state: "runnable",
        reason: "No setup recipe or lockfile-backed restore is required.",
      },
    };

    expect(isStatusCommandOutput(fixture)).toBe(true);
  });
});

function statusFixtureNames(): string[] {
  return manifestEntriesFor("status", "StatusCommandOutput").map((entry) =>
    entry.path.replace(/^status\//, "").replace(/\.json$/, ""),
  );
}

function readStatusFixture(name: string): unknown {
  return readContractFixture(`status/${name}.json`);
}

function expectRecord(value: unknown): Record<string, unknown> {
  expect(isRecord(value)).toBe(true);
  if (!isRecord(value)) {
    throw new Error("Expected fixture to be a JSON object");
  }

  return value;
}
