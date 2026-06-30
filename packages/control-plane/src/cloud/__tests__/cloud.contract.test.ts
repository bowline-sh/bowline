import { describe, expect, it } from "vitest";
import type { EventId } from "@bowline/contracts";

import {
  createDownloadIntentMetadata,
  createInMemoryCloudMetadataStore,
  createPendingDeviceAccountMapping,
  createUploadIntentMetadata,
  createWorkViewMetadata,
  validateObjectMetadata,
  type AccountId,
  type IntentId,
  type ObjectMetadataInput,
  type WorkOsOrganizationId,
  type WorkOsUserId,
} from "../index";
import type { LeaseMetadataInput } from "../types";

const workspaceId = "workspace_code" as never;
const deviceId = "device_linux" as never;
const snapshotId = "snapshot_empty" as never;
const nextSnapshotId = "snapshot_next" as never;
const contentId = "content_0011223344556677" as never;
const packId = "pk_0011223344556677" as never;
const hash = `b3_${"a".repeat(64)}`;

describe("cloud control-plane metadata contract", () => {
  it("advances workspace refs by CAS and returns typed stale refs", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_created", "event_advanced"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    const initialRef = store.createWorkspaceRef({
      snapshotId,
      workspaceId,
    });

    expect(initialRef).toEqual({
      snapshotId,
      updatedAt: "2026-06-24T12:00:00Z",
      version: 0,
      workspaceId,
    });

    const advanced = store.compareAndSwapWorkspaceRef({
      expectedVersion: 0,
      nextSnapshotId,
      updatedAt: "2026-06-24T12:01:00Z",
      writerDeviceId: deviceId,
      workspaceId,
    });

    expect(advanced).toEqual({
      event: {
        eventId: "event_advanced",
        kind: "workspace_ref.advanced",
        occurredAt: "2026-06-24T12:01:00Z",
        subject: nextSnapshotId,
        workspaceId,
      },
      ok: true,
      ref: {
        snapshotId: nextSnapshotId,
        updatedAt: "2026-06-24T12:01:00Z",
        updatedByDeviceId: deviceId,
        version: 1,
        workspaceId,
      },
    });

    const stale = store.compareAndSwapWorkspaceRef({
      expectedVersion: 0,
      nextSnapshotId: "snapshot_loser" as never,
      writerDeviceId: "device_other" as never,
      workspaceId,
    });

    expect(stale).toEqual({
      currentRef: {
        snapshotId: nextSnapshotId,
        updatedAt: "2026-06-24T12:01:00Z",
        updatedByDeviceId: deviceId,
        version: 1,
        workspaceId,
      },
      error: "stale-ref",
      ok: false,
    });
  });

  it("lists compact events and status metadata without byte payloads", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence([
        "event_created",
        "event_object",
        "event_device",
      ] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    store.createWorkspaceRef({ snapshotId, workspaceId });
    store.commitObjectMetadata(sourcePackMetadata());
    store.createPendingDeviceAccountMapping(pendingDeviceInput());

    expect(store.listEvents(workspaceId)).toEqual([
      {
        eventId: "event_created",
        kind: "workspace.created",
        occurredAt: "2026-06-24T12:00:00Z",
        subject: snapshotId,
        workspaceId,
      },
      {
        eventId: "event_object",
        kind: "object_pointer.added",
        occurredAt: "2026-06-24T12:02:00Z",
        subject: "packs_pk_0011223344556677",
        workspaceId,
      },
      {
        eventId: "event_device",
        kind: "device.approval_requested",
        occurredAt: "2026-06-24T12:03:00Z",
        subject: "device-request:workspace_code:device_linux",
        workspaceId,
      },
    ]);

    const summary = store.getCompactWorkspaceMetadata(workspaceId);
    expect(summary).toEqual({
      eventCount: 3,
      leaseCount: 0,
      latestEventId: "event_device",
      objectCount: 1,
      pendingDeviceCount: 1,
      workViewCount: 0,
      ref: {
        snapshotId,
        updatedAt: "2026-06-24T12:00:00Z",
        version: 0,
        workspaceId,
      },
      workspaceId,
    });
    expect(JSON.stringify(summary)).not.toContain("bytes");
    expect(JSON.stringify(summary)).not.toContain("tree");
  });

  it("treats repeated object metadata commits as idempotent retries", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_created", "event_object"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    store.createWorkspaceRef({ snapshotId, workspaceId });
    const first = store.commitObjectMetadata(sourcePackMetadata());
    const repeated = store.commitObjectMetadata(sourcePackMetadata());

    expect(repeated).toEqual(first);
    expect(store.listObjectMetadata(workspaceId)).toHaveLength(1);
    expect(store.listEvents(workspaceId)).toHaveLength(2);
    expect(() =>
      store.commitObjectMetadata({
        ...sourcePackMetadata(),
        byteLength: 256,
      }),
    ).toThrow(/object metadata conflict/);
  });

  it("treats repeated pending device mappings as idempotent retries", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_device"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    const first = store.createPendingDeviceAccountMapping(pendingDeviceInput());
    const repeated =
      store.createPendingDeviceAccountMapping(pendingDeviceInput());

    expect(repeated).toEqual(first);
    expect(store.getCompactWorkspaceMetadata(workspaceId)).toEqual({
      eventCount: 1,
      leaseCount: 0,
      latestEventId: "event_device",
      objectCount: 0,
      pendingDeviceCount: 1,
      workViewCount: 0,
      workspaceId,
    });
    expect(() =>
      store.createPendingDeviceAccountMapping({
        ...pendingDeviceInput(),
        matchingCode: "999999",
      }),
    ).toThrow(/pending device conflict/);
  });

  it("validates compact R2 object metadata and rejects path-derived keys", () => {
    expect(validateObjectMetadata(sourcePackMetadata())).toEqual({
      byteLength: 128,
      contentId,
      createdAt: "2026-06-24T12:02:00Z",
      createdByDeviceId: deviceId,
      hash,
      keyEpoch: 3,
      kind: "source-pack",
      objectKey: "packs_pk_0011223344556677",
      packId,
      retentionState: "pending",
      workspaceId,
    });

    expect(
      validateObjectMetadata({
        ...sourcePackMetadata(),
        objectKey: `packs_pk_${"a".repeat(64)}`,
      }),
    ).toMatchObject({
      objectKey: `packs_pk_${"a".repeat(64)}`,
    });
    expect(validateObjectMetadata(overlayPackMetadata())).toMatchObject({
      kind: "overlay-pack",
      objectKey: "packs_pk_8899aabbccddeeff",
    });
    expect(validateObjectMetadata(indexPackMetadata())).toEqual({
      byteLength: 512,
      contentId: "content_index_0011223344556677",
      createdAt: "2026-06-24T12:08:00Z",
      createdByDeviceId: deviceId,
      hash,
      keyEpoch: 3,
      kind: "index-pack",
      objectKey: "indexes_ix_0011223344556677",
      packId: "ix_0011223344556677",
      retentionState: "pending",
      workspaceId,
    });

    for (const objectKey of [
      "packs/../secret",
      "packs_.env",
      "Users_user_Code_acme",
      "packs_src_auth",
      "packs_pk_acme_web",
      "manifests_mf_scan_0011223344556677",
      "indexes_ix_acme_web",
    ]) {
      expect(() =>
        validateObjectMetadata({ ...sourcePackMetadata(), objectKey }),
      ).toThrow(/objectKey/);
      expect(() =>
        validateObjectMetadata({ ...overlayPackMetadata(), objectKey }),
      ).toThrow(/objectKey/);
      expect(() =>
        validateObjectMetadata({ ...indexPackMetadata(), objectKey }),
      ).toThrow(/objectKey/);
    }
  });

  it("stores encrypted index-pack metadata as compact object pointers", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_index_pack"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    const event = store.commitObjectMetadata(indexPackMetadata());

    expect(event).toEqual({
      eventId: "event_index_pack",
      kind: "object_pointer.added",
      occurredAt: "2026-06-24T12:08:00Z",
      subject: "indexes_ix_0011223344556677",
      workspaceId,
    });
    expect(store.listObjectMetadata(workspaceId)).toEqual([
      expect.objectContaining({
        kind: "index-pack",
        objectKey: "indexes_ix_0011223344556677",
      }),
    ]);
    expect(JSON.stringify(store.listObjectMetadata(workspaceId))).not.toMatch(
      /rawIndex|tantivy|plaintext|src\/auth/u,
    );
  });

  it("stores compact work view metadata and lifecycle events without bytes or env", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence([
        "event_created",
        "event_work_created",
        "event_work_review",
      ] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    store.createWorkspaceRef({ snapshotId, workspaceId });
    const created = store.createWorkView(workViewInput());

    expect(created).toEqual({
      event: {
        eventId: "event_work_created",
        kind: "work.created",
        occurredAt: "2026-06-24T12:04:00Z",
        subject: "work_view_spike",
        workspaceId,
      },
      workView: {
        baseSnapshotId: snapshotId,
        createdAt: "2026-06-24T12:04:00Z",
        createdByDeviceId: deviceId,
        lifecycleState: "active",
        name: "spike",
        overlayObjectKey: "packs_pk_8899aabbccddeeff",
        projectId: "project_acme",
        updatedAt: "2026-06-24T12:04:00Z",
        updatedByDeviceId: deviceId,
        version: 0,
        visiblePath: ".work/acme/spike",
        workViewId: "work_view_spike",
        workspaceId,
      },
    });

    const updated = store.updateWorkView({
      expectedVersion: 0,
      lifecycleState: "review-ready",
      reviewReadyAt: "2026-06-24T12:05:00Z",
      updatedAt: "2026-06-24T12:05:00Z",
      updatedByDeviceId: deviceId,
      workViewId: "work_view_spike",
      workspaceId,
    });

    expect(updated).toMatchObject({
      event: {
        eventId: "event_work_review",
        kind: "work.review_ready",
        occurredAt: "2026-06-24T12:05:00Z",
        subject: "work_view_spike",
        workspaceId,
      },
      ok: true,
      workView: {
        lifecycleState: "review-ready",
        reviewReadyAt: "2026-06-24T12:05:00Z",
        version: 1,
      },
    });
    expect(store.listWorkViews(workspaceId)).toHaveLength(1);
    expect(store.getCompactWorkspaceMetadata(workspaceId)).toMatchObject({
      eventCount: 3,
      leaseCount: 0,
      latestEventId: "event_work_review",
      workViewCount: 1,
    });

    const serialized = JSON.stringify(store.listWorkViews(workspaceId));
    expect(serialized).not.toContain("bytes");
    expect(serialized).not.toContain("manifestEntries");
    expect(serialized).not.toMatch(/plaintext|secret|envValue/u);
  });

  it("rejects duplicate work view names and visible paths", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_work_created"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    store.createWorkView(workViewInput());

    expect(() =>
      store.createWorkView({
        ...workViewInput(),
        name: "SPIKE",
        visiblePath: ".work/acme/spike-2",
        workViewId: "work_view_spike_2",
      }),
    ).toThrow(/work view already exists/);
    expect(() =>
      store.createWorkView({
        ...workViewInput(),
        name: "other",
        workViewId: "work_view_other",
      }),
    ).toThrow(/work view already exists/);
  });

  it("stores compact lease metadata and rejects local-only lease fields", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence([
        "event_lease_created",
        "event_lease_review",
      ] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });

    const created = store.createLease(leaseInput());
    expect(created).toEqual({
      event: {
        eventId: "event_lease_created",
        kind: "lease.created",
        occurredAt: "2026-06-24T12:06:00Z",
        subject: "lease_fix_001",
        workspaceId,
      },
      lease: {
        baseSnapshotId: snapshotId,
        createdAt: "2026-06-24T12:06:00Z",
        deviceId,
        executionState: "active",
        expiresAt: "2026-06-24T13:06:00Z",
        leaseId: "lease_fix_001",
        outputState: "empty",
        projectId: "project_acme",
        statusCode: "active",
        updatedAt: "2026-06-24T12:06:00Z",
        version: 0,
        writeTargetMode: "work-view",
        workViewId: "work_view_spike",
        workspaceId,
      },
    });

    const updated = store.updateLease({
      expectedVersion: 0,
      outputObject: leaseObjectPointer(),
      outputState: "review-ready",
      statusCode: "review-ready",
      updatedAt: "2026-06-24T12:07:00Z",
      updatedByDeviceId: deviceId,
      leaseId: "lease_fix_001",
      workspaceId,
    });
    expect(updated).toMatchObject({
      event: {
        eventId: "event_lease_review",
        kind: "lease.review_ready",
        occurredAt: "2026-06-24T12:07:00Z",
        subject: "lease_fix_001",
        workspaceId,
      },
      ok: true,
      lease: {
        outputObject: leaseObjectPointer(),
        outputState: "review-ready",
        statusCode: "review-ready",
        version: 1,
      },
    });
    expect(store.listLeases(workspaceId)).toHaveLength(1);
    expect(store.getCompactWorkspaceMetadata(workspaceId)).toMatchObject({
      eventCount: 2,
      leaseCount: 1,
      latestEventId: "event_lease_review",
    });

    const serialized = JSON.stringify(store.listLeases(workspaceId));
    expect(serialized).not.toMatch(
      /fix failing|recipe|command|prompt|src\/|review note|secret/u,
    );
    expect(() =>
      store.createLease({
        ...leaseInput(),
        task: "fix failing test",
      } as never),
    ).toThrow(/unsupported metadata field: task/);
    expect(() =>
      store.createLease({
        ...leaseInput(),
        statusCode: "review src/app.ts",
      }),
    ).toThrow(/statusCode/);
    const directStore = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_direct_created"] as EventId[]),
      now: () => "2026-06-24T12:10:00Z",
    });
    const direct = directStore.createLease({
      ...leaseInput(),
      leaseId: "lease_direct_001",
      writeTargetMode: "direct",
      workViewId: undefined,
    });
    expect(direct.lease.writeTargetMode).toBe("direct");
    expect(direct.lease.workViewId).toBeUndefined();
    expect(() =>
      directStore.createLease({
        ...leaseInput(),
        leaseId: "lease_direct_bad",
        writeTargetMode: "direct",
      }),
    ).toThrow(/direct leases/);
    expect(() =>
      store.updateLease({
        expectedVersion: 0,
        leaseId: "lease_fix_001",
        outputObject: {
          ...leaseObjectPointer(),
          objectKey: "packs_src_app_ts",
        },
        updatedByDeviceId: deviceId,
        workspaceId,
      }),
    ).toThrow(/objectKey/);
  });

  it("does not expose lease metadata when lease creation event fails", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence([]),
      now: () => "2026-06-24T12:00:00Z",
    });

    expect(() => store.createLease(leaseInput())).toThrow(
      /event id sequence exhausted/,
    );
    expect(store.listLeases(workspaceId)).toEqual([]);
    expect(store.getCompactWorkspaceMetadata(workspaceId)).toMatchObject({
      eventCount: 0,
      leaseCount: 0,
    });
  });

  it("does not expose lease updates when update event creation fails", () => {
    const store = createInMemoryCloudMetadataStore({
      nextEventId: sequence(["event_lease_created"] as EventId[]),
      now: () => "2026-06-24T12:00:00Z",
    });
    store.createLease(leaseInput());

    expect(() =>
      store.updateLease({
        expectedVersion: 0,
        leaseId: "lease_fix_001",
        outputState: "review-ready",
        statusCode: "review-ready",
        updatedAt: "2026-06-24T12:07:00Z",
        updatedByDeviceId: deviceId,
        workspaceId,
      }),
    ).toThrow(/event id sequence exhausted/);

    expect(store.listLeases(workspaceId)).toEqual([
      expect.objectContaining({
        outputState: "empty",
        statusCode: "active",
        version: 0,
      }),
    ]);
    expect(store.getCompactWorkspaceMetadata(workspaceId)).toMatchObject({
      eventCount: 1,
      leaseCount: 1,
      latestEventId: "event_lease_created",
    });
  });

  it("rejects non-normalized work view visible paths before indexing", () => {
    for (const visiblePath of [
      ".work/acme//spike",
      ".work/acme/./spike",
      ".work/acme/spike/.",
    ]) {
      expect(() =>
        createWorkViewMetadata({
          ...workViewInput(),
          visiblePath,
        }),
      ).toThrow(/visiblePath/);
    }
  });

  it("rejects path-derived work view overlay keys", () => {
    expect(() =>
      createWorkViewMetadata({
        ...workViewInput(),
        overlayObjectKey: "packs_acme_src_index_ts",
      }),
    ).toThrow(/objectKey/);

    expect(() =>
      createWorkViewMetadata({
        ...workViewInput(),
        plaintextEnv: "DATABASE_URL=postgres://example",
      } as never),
    ).toThrow(/unsupported metadata field: plaintextEnv/);
  });

  it("rejects unsupported bulk source fields", () => {
    expect(() =>
      validateObjectMetadata({
        ...sourcePackMetadata(),
        sourcePath: "~/Code/acme/.env.local",
      } as never),
    ).toThrow(/unsupported metadata field: sourcePath/);

    expect(() =>
      validateObjectMetadata({
        ...sourcePackMetadata(),
        note: "x".repeat(8_200),
      } as never),
    ).toThrow(/unsupported metadata field: note/);
  });

  it("rejects compact metadata that is too large even when fields are allowed", () => {
    expect(() =>
      validateObjectMetadata({
        ...sourcePackMetadata(),
        contentId: `content_${"a".repeat(8_200)}` as never,
      }),
    ).toThrow(/too large for compact control-plane metadata/);
  });

  it("creates upload and download intent metadata without bearer URLs", () => {
    const upload = createUploadIntentMetadata({
      byteLength: 128,
      createdAt: "2026-06-24T12:02:00Z",
      createdByDeviceId: deviceId,
      expiresAt: "2026-06-24T12:07:00Z",
      intentId: "intent_upload" as IntentId,
      kind: "source-pack",
      objectKey: "packs_pk_0011223344556677",
      workspaceId,
    });

    expect(upload).toEqual({
      byteLength: 128,
      createdAt: "2026-06-24T12:02:00Z",
      createdByDeviceId: deviceId,
      expiresAt: "2026-06-24T12:07:00Z",
      intentId: "intent_upload",
      kind: "source-pack",
      method: "PUT",
      objectKey: "packs_pk_0011223344556677",
      workspaceId,
    });

    const download = createDownloadIntentMetadata({
      createdAt: "2026-06-24T12:03:00Z",
      expiresAt: "2026-06-24T12:08:00Z",
      intentId: "intent_download" as IntentId,
      objectKey: "packs_pk_0011223344556677",
      range: { length: 64, offset: 32 },
      requestedByDeviceId: deviceId,
      workspaceId,
    });

    expect(download).toEqual({
      createdAt: "2026-06-24T12:03:00Z",
      expiresAt: "2026-06-24T12:08:00Z",
      intentId: "intent_download",
      method: "GET",
      objectKey: "packs_pk_0011223344556677",
      range: { length: 64, offset: 32 },
      requestedByDeviceId: deviceId,
      workspaceId,
    });
    expect(JSON.stringify({ download, upload })).not.toContain("https://");
    expect(JSON.stringify({ download, upload })).not.toContain("signature");
  });

  it("maps WorkOS account identity to a pending device without key material", () => {
    const mapping = createPendingDeviceAccountMapping(pendingDeviceInput());

    expect(mapping).toEqual({
      account: {
        accountId: "account_user",
        email: "user@example.com",
        workOsOrganizationId: "org_acme",
        workOsUserId: "user_test",
      },
      pendingDevice: {
        accountId: "account_user",
        decryptAuthority: "not-granted",
        deviceFingerprint: "fp_device_linux",
        deviceId,
        deviceName: "linux-server-1",
        devicePublicKey: "age1device_linux",
        expiresAt: "2026-06-24T12:13:00Z",
        matchingCode: "842113",
        platform: "linux",
        requestId: "device-request:workspace_code:device_linux",
        requestedAt: "2026-06-24T12:03:00Z",
        state: "pending",
        trustState: "pending",
        workspaceId,
      },
    });
    expect(JSON.stringify(mapping)).not.toMatch(
      /plaintext|workspaceKey|privateKey|secretKey/u,
    );

    expect(() =>
      createPendingDeviceAccountMapping({
        ...pendingDeviceInput(),
        plaintextWorkspaceKey: "do-not-return",
      } as never),
    ).toThrow(/plaintext key material/);
  });
});

function sourcePackMetadata(): ObjectMetadataInput {
  return {
    byteLength: 128,
    contentId,
    createdAt: "2026-06-24T12:02:00Z",
    createdByDeviceId: deviceId,
    hash,
    keyEpoch: 3,
    kind: "source-pack",
    objectKey: "packs_pk_0011223344556677",
    packId,
    retentionState: "pending",
    workspaceId,
  };
}

function overlayPackMetadata(): ObjectMetadataInput {
  return {
    ...sourcePackMetadata(),
    kind: "overlay-pack",
    objectKey: "packs_pk_8899aabbccddeeff",
    packId: "pk_8899aabbccddeeff" as never,
  };
}

function indexPackMetadata(): ObjectMetadataInput {
  return {
    ...sourcePackMetadata(),
    byteLength: 512,
    contentId: "content_index_0011223344556677" as never,
    createdAt: "2026-06-24T12:08:00Z",
    kind: "index-pack",
    objectKey: "indexes_ix_0011223344556677",
    packId: "ix_0011223344556677" as never,
  };
}

function workViewInput() {
  return {
    baseSnapshotId: snapshotId,
    createdAt: "2026-06-24T12:04:00Z",
    createdByDeviceId: deviceId,
    name: "spike",
    overlayObjectKey: "packs_pk_8899aabbccddeeff",
    projectId: "project_acme",
    visiblePath: ".work/acme/spike",
    workViewId: "work_view_spike",
    workspaceId,
  };
}

function leaseInput(): LeaseMetadataInput {
  return {
    baseSnapshotId: snapshotId,
    createdAt: "2026-06-24T12:06:00Z",
    deviceId,
    expiresAt: "2026-06-24T13:06:00Z",
    leaseId: "lease_fix_001",
    projectId: "project_acme",
    statusCode: "active",
    writeTargetMode: "work-view",
    workViewId: "work_view_spike",
    workspaceId,
  };
}

function leaseObjectPointer() {
  return {
    byteLength: 96,
    contentId: "content_8899aabbccddeeff" as never,
    hash: `b3_${"b".repeat(64)}`,
    keyEpoch: 3,
    kind: "overlay-pack" as const,
    objectKey: "packs_pk_8899aabbccddeeff",
  };
}

function pendingDeviceInput() {
  return {
    account: {
      accountId: "account_user" as AccountId,
      email: "user@example.com",
      workOsOrganizationId: "org_acme" as WorkOsOrganizationId,
      workOsUserId: "user_test" as WorkOsUserId,
    },
    deviceFingerprint: "fp_device_linux",
    deviceId,
    deviceName: "linux-server-1",
    devicePublicKey: "age1device_linux",
    expiresAt: "2026-06-24T12:13:00Z",
    matchingCode: "842113",
    platform: "linux" as const,
    requestId: "device-request:workspace_code:device_linux",
    requestedAt: "2026-06-24T12:03:00Z",
    workspaceId,
  };
}

function sequence(values: readonly EventId[]) {
  let index = 0;
  return () => {
    const value = values[index];
    if (value === undefined) {
      throw new Error("event id sequence exhausted");
    }
    index += 1;
    return value;
  };
}
