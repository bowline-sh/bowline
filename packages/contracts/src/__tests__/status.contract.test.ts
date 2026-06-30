import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

import {
  ACCESS_FLAGS,
  EVENT_NAMES,
  isEventName,
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

const statusFixtureNames = [
  "healthy",
  "attention",
  "limited",
  "conflict",
  "pending-device",
  "degraded-watcher",
  "active-lease",
  "review-ready-lease",
  "metadata-corrupt-limited",
  "stale-agent-base",
  "work-view-attention",
  "index-ready",
  "index-degraded",
  "sync-queue-limited",
] as const;

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
      "hydration.budget_reserved",
      "hydration.budget_committed",
      "hydration.budget_released",
      "hydration.budget_denied",
      "hydration.budget_override_granted",
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
      "work.expired",
      "work.archived",
      "work.cleanup_previewed",
      "work.cleanup_completed",
      "lease.created",
      "lease.updated",
      "lease.expired",
      "lease.completed",
      "lease.blocked",
      "lease.revoked",
      "lease.review_ready",
      "lease.tool_invoked",
      "lease.tool_denied",
      "lease.hydration_requested",
      "lease.cleanup_completed",
      "overlay.changed",
      "publish.requested",
      "conflict.created",
      "conflict.bundle_created",
      "conflict.resolution_proposed",
      "conflict.resolution_accepted",
      "conflict.resolution_rejected",
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
      "index.updated",
      "index.degraded",
      "sync.started",
      "sync.completed",
      "sync.limited",
      "sync.degraded",
      "sync.recovered",
      "watcher.degraded",
      "watcher.recovered",
      "network.offline",
      "network.recovered",
      "metadata.corrupt",
    ]);
    expect(SCHEMA_SOURCE_OF_TRUTH).toBe("hand-written-fixtures");
  });

  it.each(statusFixtureNames)(
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

  it("rejects unknown status levels and event names", () => {
    expect(() => parseStatusLevel("blocked")).toThrow(
      "Unknown status level: blocked",
    );
    expect(() => parseEventName("status.unknown")).toThrow(
      "Unknown event name: status.unknown",
    );
  });

  it("rejects status output without workspaceId", () => {
    const withoutWorkspaceId = {
      ...(readStatusFixture("healthy") as Record<string, unknown>),
    };
    delete withoutWorkspaceId.workspaceId;

    expect(isStatusCommandOutput(withoutWorkspaceId)).toBe(false);
  });
});

function readStatusFixture(name: string): unknown {
  const fixtureUrl = new URL(
    `../../../../tests/contracts/status/${name}.json`,
    import.meta.url,
  );

  return JSON.parse(readFileSync(fixtureUrl, "utf8")) as unknown;
}
