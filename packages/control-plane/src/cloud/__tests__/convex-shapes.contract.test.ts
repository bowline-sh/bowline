import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import {
  CONVEX_FUNCTIONS,
  CONVEX_TABLES,
  toConvexObjectMetadataDocument,
  toConvexPendingDeviceDocument,
  toConvexWorkspaceRefDocument,
} from "../internal/convexDocuments";
import {
  createPendingDeviceAccountMapping,
  validateObjectMetadata,
  type AccountId,
  type WorkOsOrganizationId,
  type WorkOsUserId,
} from "../index";

describe("internal Convex control-plane shapes", () => {
  it("keeps raw table and function names in the internal module", () => {
    expect(CONVEX_TABLES).toEqual({
      accountSessions: "accountSessions",
      authorizedDevices: "authorizedDevices",
      billingCheckoutAttempts: "billingCheckoutAttempts",
      billingCustomers: "billingCustomers",
      billingSubscriptions: "billingSubscriptions",
      billingWebhookEvents: "billingWebhookEvents",
      compactEventSequences: "compactEventSequences",
      compactEvents: "compactEvents",
      deviceAuthorizationProofs: "deviceAuthorizationProofs",
      encryptedDeviceGrants: "encryptedDeviceGrants",
      objectMetadata: "objectMetadata",
      pendingDeviceProofs: "pendingDeviceProofs",
      pendingDevices: "pendingDevices",
      recoveryEnvelopeProofs: "recoveryEnvelopeProofs",
      recoveryEnvelopes: "recoveryEnvelopes",
      revokedDevices: "revokedDevices",
      trustAuditEvents: "trustAuditEvents",
      workspaceAccounts: "workspaceAccounts",
      workspaceRefs: "workspaceRefs",
    });
    expect(CONVEX_FUNCTIONS.compareAndSwapWorkspaceRef).toBe(
      "refs:compareAndSwapWorkspaceRef",
    );
  });

  it("keeps hosted object manifest idempotency scoped to manifest identity", () => {
    const mutationSource = readFileSync(
      join(process.cwd(), "convex/objectMutations.ts"),
      "utf8",
    );

    expect(mutationSource).toContain(
      "existingManifest.manifestId !== args.manifestId",
    );
  });

  it("binds trusted-device bootstrap proofs to the bootstrap token hash", () => {
    const devicesSource = readFileSync(
      join(process.cwd(), "convex/devices.ts"),
      "utf8",
    );

    expect(devicesSource).toContain(
      "const tokenHash = await sha256Hex(args.bootstrapToken)",
    );
    expect(devicesSource).toContain("bootstrapTokenHash: tokenHash");
    expect(devicesSource).toContain(
      "`bootstrapTokenHash=${args.bootstrapTokenHash}`",
    );
    expect(devicesSource).toContain("bootstrapSessionProofSubject(args)");
  });

  it("keeps hosted object authority scoped by workspace", () => {
    const schemaSource = readFileSync(
      join(process.cwd(), "convex/schema.ts"),
      "utf8",
    );
    const mutationSource = readFileSync(
      join(process.cwd(), "convex/objectMutations.ts"),
      "utf8",
    );
    const querySource = readFileSync(
      join(process.cwd(), "convex/objectQueries.ts"),
      "utf8",
    );
    const r2Source = readFileSync(
      join(process.cwd(), "convex/lib/r2.ts"),
      "utf8",
    );
    const allHostedObjectSource = [mutationSource, querySource].join("\n");

    expect(schemaSource).toContain(
      '.index("by_workspace_object_key", ["workspaceId", "objectKey"])',
    );
    expect(schemaSource).toContain(
      '.index("by_workspace_manifest", ["workspaceId", "manifestId"])',
    );
    expect(allHostedObjectSource).toContain("by_workspace_object_key");
    expect(allHostedObjectSource).toContain("by_workspace_manifest");
    expect(allHostedObjectSource).not.toContain("by_object_key");
    expect(allHostedObjectSource).not.toContain("by_manifest_id");

    expect(r2Source).toContain("physicalObjectKey(workspaceId, objectKey)");
    expect(r2Source).toContain("workspaces/${workspacePrefix}/${objectKey}");
    expect(r2Source).toContain('digest("hex")');
    expect(r2Source).not.toContain("Key: objectKey");
  });

  it("keeps hosted delete authority fail-closed behind retained-reference checks", () => {
    const objectsSource = readFileSync(
      join(process.cwd(), "convex/objects.ts"),
      "utf8",
    );
    const mutationSource = readFileSync(
      join(process.cwd(), "convex/objectMutations.ts"),
      "utf8",
    );
    const r2Source = readFileSync(
      join(process.cwd(), "convex/lib/r2.ts"),
      "utf8",
    );

    expect(objectsSource).toContain("export const createDeleteIntent = action");
    expect(objectsSource).toContain('proofAction: "create-delete-intent"');
    expect(objectsSource).toContain("retentionState=delete-eligible");
    expect(objectsSource).toContain(
      "hosted delete intent is disabled until hosted GC can perform deletion with a final liveness check",
    );
    expect(objectsSource).not.toContain("createDeleteUrl(args.workspaceId");
    expect(objectsSource).toContain("mark-object-retention-state");
    expect(objectsSource).toContain(
      "delete-eligible retention requires GC authority",
    );
    expect(objectsSource).toContain("requireAuthorizedDevice");

    expect(mutationSource).toContain(
      'object.retentionState !== "delete-eligible"',
    );
    expect(mutationSource).toContain(
      "delete intent key epoch does not match metadata",
    );
    expect(mutationSource).toContain(
      "delete intent object kind does not match metadata",
    );
    expect(mutationSource).toContain("assertObjectNotLiveReferenced");
    expect(mutationSource).toContain(
      "object is referenced by the current workspace ref",
    );
    expect(mutationSource).toContain(
      "object is referenced by a retained workspace manifest",
    );
    expect(mutationSource).toContain(
      'await ctx.db.patch(existing._id, { retentionState: "current" })',
    );
    expect(mutationSource).toContain("by_workspace_object_key");

    expect(r2Source).toContain("DeleteObjectCommand");
    expect(r2Source).toContain("createDeleteUrl");
    expect(r2Source).toContain("physicalObjectKey(workspaceId, objectKey)");
    expect(r2Source).toContain("const SIGNED_URL_EXPIRY_SECONDS = 300");
    expect(r2Source).toContain("expiresIn: SIGNED_URL_EXPIRY_SECONDS");
  });

  it("serializes workspace refs and object metadata as compact documents", () => {
    expect(
      toConvexWorkspaceRefDocument({
        snapshotId: "snapshot_next" as never,
        updatedAt: "2026-06-24T12:01:00Z",
        updatedByDeviceId: "device_linux" as never,
        version: 1,
        workspaceId: "workspace_code" as never,
      }),
    ).toEqual({
      snapshotId: "snapshot_next",
      updatedAt: "2026-06-24T12:01:00Z",
      updatedByDeviceId: "device_linux",
      version: 1,
      workspaceId: "workspace_code",
    });

    const objectDocument = toConvexObjectMetadataDocument(
      validateObjectMetadata({
        byteLength: 128,
        contentId: "content_0011223344556677" as never,
        createdAt: "2026-06-24T12:02:00Z",
        createdByDeviceId: "device_linux" as never,
        hash: `b3_${"a".repeat(64)}`,
        keyEpoch: 3,
        kind: "source-pack",
        objectKey: "packs_pk_0011223344556677",
        packId: "pk_0011223344556677" as never,
        retentionState: "pending",
        workspaceId: "workspace_code" as never,
      }),
    );

    expect(objectDocument).toEqual({
      byteLength: 128,
      contentId: "content_0011223344556677",
      createdAt: "2026-06-24T12:02:00Z",
      createdByDeviceId: "device_linux",
      hash: `b3_${"a".repeat(64)}`,
      keyEpoch: 3,
      kind: "source-pack",
      objectKey: "packs_pk_0011223344556677",
      packId: "pk_0011223344556677",
      retentionState: "pending",
      workspaceId: "workspace_code",
    });
    expect(JSON.stringify(objectDocument)).not.toContain("sourcePath");
    expect(JSON.stringify(objectDocument)).not.toContain("bytes");

    const overlayDocument = toConvexObjectMetadataDocument(
      validateObjectMetadata({
        byteLength: 96,
        contentId: "content_8899aabbccddeeff" as never,
        createdAt: "2026-06-24T12:04:00Z",
        createdByDeviceId: "device_linux" as never,
        hash: `b3_${"b".repeat(64)}`,
        keyEpoch: 3,
        kind: "overlay-pack",
        objectKey: "packs_pk_8899aabbccddeeff",
        packId: "pk_8899aabbccddeeff" as never,
        retentionState: "pending",
        workspaceId: "workspace_code" as never,
      }),
    );

    expect(overlayDocument).toMatchObject({
      kind: "overlay-pack",
      objectKey: "packs_pk_8899aabbccddeeff",
    });

    const indexDocument = toConvexObjectMetadataDocument(
      validateObjectMetadata({
        byteLength: 512,
        contentId: "content_index_0011223344556677" as never,
        createdAt: "2026-06-24T12:08:00Z",
        createdByDeviceId: "device_linux" as never,
        hash: `b3_${"c".repeat(64)}`,
        keyEpoch: 3,
        kind: "index-pack",
        objectKey: "indexes_ix_0011223344556677",
        packId: "ix_0011223344556677" as never,
        retentionState: "pending",
        workspaceId: "workspace_code" as never,
      }),
    );

    expect(indexDocument).toMatchObject({
      kind: "index-pack",
      objectKey: "indexes_ix_0011223344556677",
      packId: "ix_0011223344556677",
    });
    const locatorDocument = toConvexObjectMetadataDocument(
      validateObjectMetadata({
        byteLength: 768,
        contentId: "content_locator_0011223344556677" as never,
        createdAt: "2026-06-24T12:09:00Z",
        createdByDeviceId: "device_linux" as never,
        hash: `b3_${"d".repeat(64)}`,
        keyEpoch: 3,
        kind: "locator-index",
        objectKey: "indexes_ix_8899aabbccddeeff",
        packId: "ix_8899aabbccddeeff" as never,
        retentionState: "pending",
        workspaceId: "workspace_code" as never,
      }),
    );

    expect(locatorDocument).toMatchObject({
      kind: "locator-index",
      objectKey: "indexes_ix_8899aabbccddeeff",
      packId: "ix_8899aabbccddeeff",
    });
    expect(JSON.stringify(indexDocument)).not.toMatch(
      /rawIndex|tantivy|plaintext|src\/auth/u,
    );
    expect(JSON.stringify(locatorDocument)).not.toMatch(
      /rawIndex|tantivy|plaintext|src\/auth/u,
    );
    expect(() =>
      validateObjectMetadata({
        ...overlayDocument,
        objectKey: "packs_acme_src_index_ts",
      } as never),
    ).toThrow(/objectKey/);
  });

  it("defines Convex work view persistence with compact metadata only", () => {
    const schemaSource = readFileSync(
      join(process.cwd(), "convex/schema.ts"),
      "utf8",
    );
    const objectsSource = readFileSync(
      join(process.cwd(), "convex/objects.ts"),
      "utf8",
    );
    const objectMutationsSource = readFileSync(
      join(process.cwd(), "convex/objectMutations.ts"),
      "utf8",
    );
    const objectKeysSource = readFileSync(
      join(process.cwd(), "convex/lib/objectKeys.ts"),
      "utf8",
    );
    const workViewSource = readFileSync(
      join(process.cwd(), "convex/workViews.ts"),
      "utf8",
    );

    expect(schemaSource).toContain("workViews: defineTable");
    expect(schemaSource).toContain('v.literal("overlay-pack")');
    expect(schemaSource).toContain('v.literal("index-pack")');
    expect(schemaSource).toContain('v.literal("locator-index")');
    expect(objectsSource).toContain('v.literal("index-pack")');
    expect(objectsSource).toContain('v.literal("locator-index")');
    expect(objectMutationsSource).toContain('v.literal("index-pack")');
    expect(objectMutationsSource).toContain('v.literal("locator-index")');
    expect(objectKeysSource).toContain("indexes_ix_");
    expect(schemaSource).toContain(
      '.index("by_workspace_project", ["workspaceId", "projectId"])',
    );
    expect(schemaSource).toContain(
      '.index("by_workspace_visible_path", ["workspaceId", "visiblePath"])',
    );
    for (const eventKind of [
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
    ]) {
      expect(schemaSource).toContain(eventKind);
    }

    expect(workViewSource).toContain("export const createWorkView");
    expect(workViewSource).toContain("export const updateWorkView");
    expect(workViewSource).toContain("export const listWorkViews");
    expect(workViewSource).toContain(
      'assertObjectKey(args.overlayObjectKey, "overlay-pack")',
    );
    expect(workViewSource).not.toContain("requireCurrentBaseRef");
    expect(workViewSource).toContain("requireKnownBaseSnapshot");
    expect(workViewSource).toContain("base snapshot has not been committed");
    expect(workViewSource.indexOf("if (existing !== null)")).toBeLessThan(
      workViewSource.indexOf("await requireKnownBaseSnapshot"),
    );
    expect(workViewSource).toContain("by_workspace_snapshot");
    expect(workViewSource).not.toContain("by_workspace_manifest");
    expect(workViewSource).toContain("requireCommittedOverlayObject");
    expect(workViewSource).toContain("rejectDuplicateWorkView");
    expect(workViewSource).toContain("by_workspace_project");
    expect(workViewSource).toContain("by_workspace_visible_path");
    expect(workViewSource).toContain("segments.some");
    expect(workViewSource).not.toMatch(
      /rawBytes|bytesBase64|manifestEntries|plaintextEnv|envValue/u,
    );
  });

  it("defines Convex lease persistence with compact allowlisted metadata only", () => {
    const schemaSource = readFileSync(
      join(process.cwd(), "convex/schema.ts"),
      "utf8",
    );
    const eventsSource = readFileSync(
      join(process.cwd(), "convex/events.ts"),
      "utf8",
    );

    expect(schemaSource).toContain("agentLeases: defineTable");
    expect(schemaSource).toContain("leaseExecutionState");
    expect(schemaSource).toContain("leaseOutputState");
    expect(schemaSource).toContain('v.literal("overlay-pack")');
    expect(schemaSource).toContain(
      '.index("by_workspace_lease", ["workspaceId", "leaseId"])',
    );
    expect(schemaSource).toContain(
      '.index("by_workspace_work_view", ["workspaceId", "workViewId"])',
    );
    for (const eventKind of [
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
      "overlay.changed",
      "publish.requested",
      "lease.cleanup_completed",
    ]) {
      expect(schemaSource).toContain(eventKind);
    }

    expect(eventsSource).toContain("export const createLease");
    expect(eventsSource).toContain("export const updateLease");
    expect(eventsSource).toContain("export const listLeases");
    expect(eventsSource).toContain("assertTrustedDevice");
    expect(eventsSource).toContain(
      'assertObjectKey(pointer.objectKey, "overlay-pack")',
    );
    expect(eventsSource).toContain(
      'leasePointerProofSubject("outputObject", args.outputObject)',
    );
    expect(eventsSource).toContain(
      'leasePointerProofSubject("auditObject", args.auditObject)',
    );
    expect(eventsSource).toContain("requireCommittedLeaseObject");
    expect(eventsSource).toContain(
      'throw new Error("lease object has not been committed")',
    );
    expect(eventsSource).not.toMatch(
      /rawTask|taskText|recipeName|commandSummary|promptText|visiblePath|reviewNote|secret/u,
    );

    const document = {
      baseSnapshotId: "snapshot_empty",
      createdAt: "2026-06-24T12:06:00Z",
      deviceId: "device_linux",
      executionState: "active",
      expiresAt: "2026-06-24T13:06:00Z",
      leaseId: "lease_fix_001",
      outputObject: {
        byteLength: 96,
        contentId: "content_8899aabbccddeeff",
        hash: `b3_${"b".repeat(64)}`,
        kind: "overlay-pack",
        objectKey: "packs_pk_8899aabbccddeeff",
      },
      outputState: "review-ready",
      projectId: "project_acme",
      statusCode: "review-ready",
      updatedAt: "2026-06-24T12:07:00Z",
      version: 1,
      workViewId: "work_view_spike",
      workspaceId: "workspace_code",
    };

    expect(document).toMatchObject({
      leaseId: "lease_fix_001",
      outputObject: {
        kind: "overlay-pack",
        objectKey: "packs_pk_8899aabbccddeeff",
      },
      outputState: "review-ready",
      statusCode: "review-ready",
    });
    expect(JSON.stringify(document)).not.toMatch(
      /fix failing|recipe|command|prompt|src\/|review note|secret/u,
    );
  });

  it("serializes pending devices without plaintext key material", () => {
    const document = toConvexPendingDeviceDocument(
      createPendingDeviceAccountMapping({
        account: {
          accountId: "account_theo" as AccountId,
          workOsOrganizationId: "org_acme" as WorkOsOrganizationId,
          workOsUserId: "user_theo" as WorkOsUserId,
        },
        deviceFingerprint: "fp_device_linux",
        deviceId: "device_linux" as never,
        deviceName: "linux-server-1",
        devicePublicKey: "age1device_linux",
        expiresAt: "2026-06-24T12:13:00Z",
        matchingCode: "842113",
        platform: "linux",
        requestId: "device-request:workspace_code:device_linux",
        requestedAt: "2026-06-24T12:03:00Z",
        workspaceId: "workspace_code" as never,
      }),
    );

    expect(document).toEqual({
      accountId: "account_theo",
      decryptAuthority: "not-granted",
      deviceFingerprint: "fp_device_linux",
      deviceId: "device_linux",
      deviceName: "linux-server-1",
      devicePublicKey: "age1device_linux",
      expiresAt: "2026-06-24T12:13:00Z",
      matchingCode: "842113",
      platform: "linux",
      requestId: "device-request:workspace_code:device_linux",
      requestedAt: "2026-06-24T12:03:00Z",
      state: "pending",
      trustState: "pending",
      workOsOrganizationId: "org_acme",
      workOsUserId: "user_theo",
      workspaceId: "workspace_code",
    });
    expect(JSON.stringify(document)).not.toMatch(
      /plaintext|workspaceKey|privateKey|secretKey/u,
    );
  });
});
