import { Buffer } from "node:buffer";

import type { EventId } from "@bowline/contracts/ids";

import type {
  CloudMetadataStore,
  CompactEvent,
  CompactEventKind,
  CompareAndSwapWorkspaceRefInput,
  CompareAndSwapWorkspaceRefResult,
  CreateWorkspaceRefInput,
  LeaseEventKind,
  LeaseExecutionState,
  LeaseMetadata,
  LeaseMetadataInput,
  LeaseObjectPointer,
  LeaseOutputState,
  LeaseWriteTargetMode,
  ObjectMetadata,
  ObjectMetadataInput,
  PendingDeviceAccountInput,
  PendingDeviceAccountMapping,
  UpdateLeaseInput,
  UpdateWorkViewInput,
  WorkspaceRef,
  WorkViewEventKind,
  WorkViewMetadata,
  WorkViewMetadataInput,
} from "../types";
import {
  createWorkViewMetadata,
  createPendingDeviceAccountMapping,
  validateObjectMetadata,
  validateWorkViewUpdateInput,
} from "./validation";

type StoreOptions = {
  readonly now?: () => string;
  readonly nextEventId?: () => EventId;
};

type ValidatedLeaseUpdate = {
  readonly workspaceId: UpdateLeaseInput["workspaceId"];
  readonly leaseId: string;
  readonly expectedVersion: number;
  readonly updatedByDeviceId: UpdateLeaseInput["updatedByDeviceId"];
  readonly updatedAt?: string;
  readonly executionState?: LeaseExecutionState;
  readonly outputState?: LeaseOutputState;
  readonly statusCode?: string;
  readonly outputObject?: LeaseObjectPointer;
  readonly auditObject?: LeaseObjectPointer;
  readonly eventKind?: LeaseEventKind;
};

export function createInMemoryCloudMetadataStore(
  options: StoreOptions = {},
): CloudMetadataStore {
  const now = options.now ?? (() => new Date().toISOString());
  const nextEventId = options.nextEventId ?? createSequentialEventIdGenerator();
  const refs = new Map<string, WorkspaceRef>();
  const events = new Map<string, CompactEvent[]>();
  const objects = new Map<string, ObjectMetadata[]>();
  const objectEvents = new Map<string, CompactEvent>();
  const pendingDevices = new Map<string, PendingDeviceAccountMapping[]>();
  const workViews = new Map<string, WorkViewMetadata[]>();
  const workViewEvents = new Map<string, CompactEvent>();
  const leases = new Map<string, LeaseMetadata[]>();
  const leaseEvents = new Map<string, CompactEvent>();

  function appendEvent(
    workspaceId: WorkspaceRef["workspaceId"],
    kind: CompactEventKind,
    subject: string,
    occurredAt = now(),
  ): CompactEvent {
    const event: CompactEvent = {
      eventId: nextEventId(),
      kind,
      occurredAt,
      subject,
      workspaceId,
    };
    const workspaceEvents = events.get(workspaceId) ?? [];
    workspaceEvents.push(event);
    events.set(workspaceId, workspaceEvents);
    return event;
  }

  return {
    createPendingDeviceAccountMapping(input: PendingDeviceAccountInput) {
      const mapping = createPendingDeviceAccountMapping(input);
      const workspaceMappings = pendingDevices.get(input.workspaceId) ?? [];
      const pendingKey = pendingDeviceKey(mapping);
      const existing = workspaceMappings.find(
        (item) => pendingDeviceKey(item) === pendingKey,
      );

      if (existing !== undefined) {
        if (!pendingDeviceMappingMatches(existing, mapping)) {
          throw new Error(
            `pending device conflict for ${input.workspaceId}/${input.deviceId}`,
          );
        }
        return existing;
      }

      workspaceMappings.push(mapping);
      pendingDevices.set(input.workspaceId, workspaceMappings);
      appendEvent(
        input.workspaceId,
        "device.approval_requested",
        mapping.pendingDevice.requestId,
        input.requestedAt,
      );
      return mapping;
    },

    createWorkspaceRef(input: CreateWorkspaceRefInput) {
      const existing = refs.get(input.workspaceId);
      if (existing !== undefined) return existing;

      const ref: WorkspaceRef = {
        snapshotId: input.snapshotId,
        updatedAt: input.createdAt ?? now(),
        version: 0,
        workspaceId: input.workspaceId,
        ...(input.createdByDeviceId === undefined
          ? {}
          : { updatedByDeviceId: input.createdByDeviceId }),
      };
      refs.set(input.workspaceId, ref);
      appendEvent(
        input.workspaceId,
        "workspace.created",
        input.snapshotId,
        ref.updatedAt,
      );
      return ref;
    },

    getWorkspaceRef(workspaceId) {
      return refs.get(workspaceId);
    },

    compareAndSwapWorkspaceRef(
      input: CompareAndSwapWorkspaceRefInput,
    ): CompareAndSwapWorkspaceRefResult {
      const currentRef = refs.get(input.workspaceId);
      if (currentRef === undefined) {
        return {
          error: "workspace-missing",
          ok: false,
          workspaceId: input.workspaceId,
        };
      }

      if (currentRef.version !== input.expectedVersion) {
        return { currentRef, error: "stale-ref", ok: false };
      }

      const nextRef: WorkspaceRef = {
        snapshotId: input.nextSnapshotId,
        updatedAt: input.updatedAt ?? now(),
        updatedByDeviceId: input.writerDeviceId,
        version: currentRef.version + 1,
        workspaceId: input.workspaceId,
      };
      refs.set(input.workspaceId, nextRef);
      const event = appendEvent(
        input.workspaceId,
        "workspace_ref.advanced",
        input.nextSnapshotId,
        nextRef.updatedAt,
      );

      return { event, ok: true, ref: nextRef };
    },

    commitObjectMetadata(input: ObjectMetadataInput) {
      const metadata = validateObjectMetadata(input);
      const workspaceObjects = objects.get(metadata.workspaceId) ?? [];
      const objectEventKey = metadataEventKey(
        metadata.workspaceId,
        metadata.objectKey,
      );
      const existing = workspaceObjects.find(
        (object) => object.objectKey === metadata.objectKey,
      );

      if (existing !== undefined) {
        if (!objectMetadataMatches(existing, metadata)) {
          throw new Error(
            `object metadata conflict for ${metadata.workspaceId}/${metadata.objectKey}`,
          );
        }
        const existingEvent = objectEvents.get(objectEventKey);
        if (existingEvent !== undefined) return existingEvent;
      }

      workspaceObjects.push(metadata);
      objects.set(metadata.workspaceId, workspaceObjects);
      const event = appendEvent(
        metadata.workspaceId,
        "object_pointer.added",
        metadata.objectKey,
        metadata.createdAt,
      );
      objectEvents.set(objectEventKey, event);
      return event;
    },

    createWorkView(input: WorkViewMetadataInput) {
      const workView = createWorkViewMetadata(input);
      const workspaceWorkViews = workViews.get(workView.workspaceId) ?? [];
      const workViewEventKey = metadataEventKey(
        workView.workspaceId,
        workView.workViewId,
      );
      const existing = workspaceWorkViews.find(
        (item) => item.workViewId === workView.workViewId,
      );

      if (existing !== undefined) {
        if (!workViewMetadataMatches(existing, workView)) {
          throw new Error(
            `work view metadata conflict for ${workView.workspaceId}/${workView.workViewId}`,
          );
        }
        const existingEvent = workViewEvents.get(workViewEventKey);
        if (existingEvent !== undefined) {
          return { event: existingEvent, workView: existing };
        }
      }
      rejectDuplicateWorkView(workspaceWorkViews, workView);

      workspaceWorkViews.push(workView);
      workViews.set(workView.workspaceId, workspaceWorkViews);
      const event = appendEvent(
        workView.workspaceId,
        "work.created",
        workView.workViewId,
        workView.createdAt,
      );
      workViewEvents.set(workViewEventKey, event);
      return { event, workView };
    },

    updateWorkView(input: UpdateWorkViewInput) {
      const update = validateWorkViewUpdateInput(input);
      const workspaceWorkViews = workViews.get(update.workspaceId) ?? [];
      const existingIndex = workspaceWorkViews.findIndex(
        (item) => item.workViewId === update.workViewId,
      );

      if (existingIndex < 0) {
        return {
          error: "work-view-missing",
          ok: false,
          workViewId: input.workViewId,
          workspaceId: update.workspaceId,
        };
      }

      const existing = workspaceWorkViews[existingIndex];
      if (existing === undefined) {
        throw new Error("work view index disappeared");
      }
      if (existing.version !== update.expectedVersion) {
        return {
          currentWorkView: existing,
          error: "stale-work-view",
          ok: false,
        };
      }

      const updatedAt = update.updatedAt ?? now();
      const workView: WorkViewMetadata = {
        ...existing,
        updatedAt,
        updatedByDeviceId: update.updatedByDeviceId,
        version: existing.version + 1,
        ...(update.lifecycleState === undefined
          ? {}
          : { lifecycleState: update.lifecycleState }),
        ...(update.overlayObjectKey === undefined
          ? {}
          : { overlayObjectKey: update.overlayObjectKey }),
        ...(update.overlayManifestId === undefined
          ? {}
          : { overlayManifestId: update.overlayManifestId }),
        ...(update.expiresAt === undefined
          ? {}
          : { expiresAt: update.expiresAt }),
        ...(update.retainUntil === undefined
          ? {}
          : { retainUntil: update.retainUntil }),
        ...(update.reviewReadyAt === undefined
          ? {}
          : { reviewReadyAt: update.reviewReadyAt }),
      };
      workspaceWorkViews[existingIndex] = workView;
      workViews.set(workView.workspaceId, workspaceWorkViews);

      const event = appendEvent(
        workView.workspaceId,
        update.eventKind ?? workViewEventKind(existing, workView),
        workView.workViewId,
        updatedAt,
      );
      return { event, ok: true, workView };
    },

    createLease(input: LeaseMetadataInput) {
      const lease = createLeaseMetadata(input);
      const workspaceLeases = leases.get(lease.workspaceId) ?? [];
      const leaseEventKey = metadataEventKey(lease.workspaceId, lease.leaseId);
      const existing = workspaceLeases.find(
        (item) => item.leaseId === lease.leaseId,
      );

      if (existing !== undefined) {
        if (!leaseMetadataMatches(existing, lease)) {
          throw new Error(
            `lease metadata conflict for ${lease.workspaceId}/${lease.leaseId}`,
          );
        }
        const existingEvent = leaseEvents.get(leaseEventKey);
        if (existingEvent !== undefined) return { event: existingEvent, lease };
        throw new Error(
          `lease metadata is missing creation event for ${lease.workspaceId}/${lease.leaseId}`,
        );
      }

      const event = appendEvent(
        lease.workspaceId,
        "lease.created",
        lease.leaseId,
        lease.createdAt,
      );
      leaseEvents.set(leaseEventKey, event);
      workspaceLeases.push(lease);
      leases.set(lease.workspaceId, workspaceLeases);
      return { event, lease };
    },

    updateLease(input: UpdateLeaseInput) {
      const update = validateLeaseUpdateInput(input);
      const workspaceLeases = leases.get(update.workspaceId) ?? [];
      const existingIndex = workspaceLeases.findIndex(
        (item) => item.leaseId === update.leaseId,
      );

      if (existingIndex < 0) {
        return {
          error: "lease-missing",
          ok: false,
          leaseId: update.leaseId,
          workspaceId: update.workspaceId,
        };
      }

      const existing = workspaceLeases[existingIndex];
      if (existing === undefined) throw new Error("lease index disappeared");
      if (existing.version !== update.expectedVersion) {
        return {
          currentLease: existing,
          error: "stale-lease",
          ok: false,
        };
      }

      const updatedAt = update.updatedAt ?? now();
      const lease: LeaseMetadata = {
        ...existing,
        updatedAt,
        version: existing.version + 1,
        ...(update.auditObject === undefined
          ? {}
          : { auditObject: update.auditObject }),
        ...(update.executionState === undefined
          ? {}
          : { executionState: update.executionState }),
        ...(update.outputObject === undefined
          ? {}
          : { outputObject: update.outputObject }),
        ...(update.outputState === undefined
          ? {}
          : { outputState: update.outputState }),
        ...(update.statusCode === undefined
          ? {}
          : { statusCode: update.statusCode }),
      };
      const event = appendEvent(
        lease.workspaceId,
        update.eventKind ?? leaseEventKind(lease),
        lease.leaseId,
        updatedAt,
      );
      workspaceLeases[existingIndex] = lease;
      leases.set(lease.workspaceId, workspaceLeases);
      return { event, ok: true, lease };
    },

    getCompactWorkspaceMetadata(workspaceId) {
      const workspaceEvents = events.get(workspaceId) ?? [];
      const latestEvent = workspaceEvents.at(-1);
      const ref = refs.get(workspaceId);
      return {
        eventCount: workspaceEvents.length,
        leaseCount: leases.get(workspaceId)?.length ?? 0,
        objectCount: objects.get(workspaceId)?.length ?? 0,
        pendingDeviceCount: pendingDevices.get(workspaceId)?.length ?? 0,
        workViewCount: workViews.get(workspaceId)?.length ?? 0,
        workspaceId,
        ...(ref === undefined ? {} : { ref }),
        ...(latestEvent === undefined
          ? {}
          : { latestEventId: latestEvent.eventId }),
      };
    },

    listEvents(workspaceId) {
      return [...(events.get(workspaceId) ?? [])];
    },

    listObjectMetadata(workspaceId) {
      return [...(objects.get(workspaceId) ?? [])];
    },

    listWorkViews(workspaceId) {
      return [...(workViews.get(workspaceId) ?? [])];
    },

    listLeases(workspaceId) {
      return [...(leases.get(workspaceId) ?? [])];
    },
  };
}

function metadataEventKey(workspaceId: string, objectKey: string): string {
  return `${workspaceId}\0${objectKey}`;
}

function createLeaseMetadata(input: LeaseMetadataInput): LeaseMetadata {
  assertAllowedKeys(
    input,
    new Set([
      "workspaceId",
      "leaseId",
      "projectId",
      "deviceId",
      "writeTargetMode",
      "workViewId",
      "baseSnapshotId",
      "createdAt",
      "expiresAt",
      "executionState",
      "outputState",
      "statusCode",
      "outputObject",
      "auditObject",
    ]),
    "lease metadata",
  );

  const metadata: LeaseMetadata = {
    baseSnapshotId: validateCompactId(
      input.baseSnapshotId,
      "baseSnapshotId",
    ) as LeaseMetadata["baseSnapshotId"],
    createdAt: input.createdAt,
    deviceId: validateCompactId(
      input.deviceId,
      "deviceId",
    ) as LeaseMetadata["deviceId"],
    executionState:
      input.executionState === undefined
        ? "active"
        : validateLeaseExecutionState(input.executionState),
    expiresAt: input.expiresAt,
    leaseId: validateCompactId(input.leaseId, "leaseId"),
    outputState:
      input.outputState === undefined
        ? "empty"
        : validateLeaseOutputState(input.outputState),
    projectId: validateCompactId(input.projectId, "projectId"),
    statusCode: validateStatusCode(input.statusCode),
    updatedAt: input.createdAt,
    version: 0,
    writeTargetMode: validateLeaseWriteTargetMode(input.writeTargetMode),
    workspaceId: input.workspaceId,
    ...(input.workViewId === undefined
      ? {}
      : {
          workViewId: validateCompactId(
            input.workViewId,
            "workViewId",
          ) as LeaseMetadata["workViewId"],
        }),
    ...(input.auditObject === undefined
      ? {}
      : { auditObject: validateLeaseObjectPointer(input.auditObject) }),
    ...(input.outputObject === undefined
      ? {}
      : { outputObject: validateLeaseObjectPointer(input.outputObject) }),
  };
  assertCompactLeaseDocument("lease metadata", metadata);
  assertLeaseWriteTarget(metadata.writeTargetMode, metadata.workViewId);
  return metadata;
}

function validateLeaseUpdateInput(
  input: UpdateLeaseInput,
): ValidatedLeaseUpdate {
  assertAllowedKeys(
    input,
    new Set([
      "workspaceId",
      "leaseId",
      "expectedVersion",
      "updatedAt",
      "updatedByDeviceId",
      "executionState",
      "outputState",
      "statusCode",
      "outputObject",
      "auditObject",
      "eventKind",
    ]),
    "lease update",
  );
  assertSafeInteger(input.expectedVersion, "lease expectedVersion");

  const update: ValidatedLeaseUpdate = {
    expectedVersion: input.expectedVersion,
    leaseId: validateCompactId(input.leaseId, "leaseId"),
    updatedByDeviceId: validateCompactId(
      input.updatedByDeviceId,
      "deviceId",
    ) as UpdateLeaseInput["updatedByDeviceId"],
    workspaceId: input.workspaceId,
    ...(input.auditObject === undefined
      ? {}
      : { auditObject: validateLeaseObjectPointer(input.auditObject) }),
    ...(input.eventKind === undefined
      ? {}
      : { eventKind: validateLeaseEventKind(input.eventKind) }),
    ...(input.executionState === undefined
      ? {}
      : { executionState: validateLeaseExecutionState(input.executionState) }),
    ...(input.outputObject === undefined
      ? {}
      : { outputObject: validateLeaseObjectPointer(input.outputObject) }),
    ...(input.outputState === undefined
      ? {}
      : { outputState: validateLeaseOutputState(input.outputState) }),
    ...(input.statusCode === undefined
      ? {}
      : { statusCode: validateStatusCode(input.statusCode) }),
    ...(input.updatedAt === undefined ? {} : { updatedAt: input.updatedAt }),
  };
  assertCompactLeaseDocument("lease update", update);
  return update;
}

function objectMetadataMatches(
  left: ObjectMetadata,
  right: ObjectMetadata,
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function workViewMetadataMatches(
  left: WorkViewMetadata,
  right: WorkViewMetadata,
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function leaseMetadataMatches(
  left: LeaseMetadata,
  right: LeaseMetadata,
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function rejectDuplicateWorkView(
  existing: WorkViewMetadata[],
  next: WorkViewMetadata,
): void {
  const normalizedName = next.name.toLowerCase();
  const duplicate = existing.find(
    (workView) =>
      (workView.projectId === next.projectId &&
        workView.name.toLowerCase() === normalizedName) ||
      workView.visiblePath === next.visiblePath,
  );
  if (duplicate !== undefined) {
    throw new Error(
      `work view already exists for ${next.workspaceId}/${next.projectId}/${next.name}`,
    );
  }
}

function workViewEventKind(
  before: WorkViewMetadata,
  after: WorkViewMetadata,
): WorkViewEventKind {
  if (before.lifecycleState === after.lifecycleState) return "work.updated";
  if (after.lifecycleState === "review-ready") return "work.review_ready";
  if (after.lifecycleState === "accepted") return "work.accepted";
  if (after.lifecycleState === "discarded") return "work.discarded";
  if (after.lifecycleState === "expired") return "work.expired";
  if (after.lifecycleState === "archived") return "work.archived";
  return "work.restored";
}

function leaseEventKind(lease: LeaseMetadata): CompactEventKind {
  if (lease.outputState === "review-ready") return "lease.review_ready";
  if (lease.executionState === "blocked") return "lease.blocked";
  if (lease.executionState === "completed") return "lease.completed";
  if (lease.executionState === "expired") return "lease.expired";
  if (lease.executionState === "revoked") return "lease.revoked";
  return "lease.updated";
}

function validateCompactId(
  value: string,
  label: string,
  maxLength = 160,
): string {
  if (
    value.length === 0 ||
    value.length > maxLength ||
    value.includes("/") ||
    value.includes("\\") ||
    value.includes(".") ||
    !/^[A-Za-z0-9:_-]+$/u.test(value)
  ) {
    throw new Error(`${label} must be a compact pathless identifier`);
  }
  return value;
}

function validateStatusCode(value: string): string {
  return validateCompactId(value, "statusCode", 80);
}

function validateLeaseExecutionState(value: string): LeaseExecutionState {
  if (
    !["active", "blocked", "completed", "expired", "revoked"].includes(value)
  ) {
    throw new Error(`unsupported lease executionState: ${value}`);
  }
  return value as LeaseExecutionState;
}

function validateLeaseOutputState(value: string): LeaseOutputState {
  if (
    ![
      "empty",
      "dirty",
      "review-ready",
      "accepted",
      "discarded",
      "conflicted",
      "retained",
    ].includes(value)
  ) {
    throw new Error(`unsupported lease outputState: ${value}`);
  }
  return value as LeaseOutputState;
}

function validateLeaseWriteTargetMode(value: string): LeaseWriteTargetMode {
  if (value !== "direct" && value !== "work-view") {
    throw new Error(`unsupported lease writeTargetMode: ${value}`);
  }
  return value;
}

function assertLeaseWriteTarget(
  mode: LeaseWriteTargetMode,
  workViewId: LeaseMetadata["workViewId"],
): void {
  if (mode === "direct") {
    if (workViewId !== undefined) {
      throw new Error("direct leases must not carry workViewId");
    }
    return;
  }
  if (workViewId === undefined) {
    throw new Error("work-view leases require workViewId");
  }
}

function validateLeaseEventKind(value: string): LeaseEventKind {
  if (
    ![
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
    ].includes(value)
  ) {
    throw new Error(`unsupported lease event kind: ${value}`);
  }
  return value as LeaseEventKind;
}

function validateLeaseObjectPointer(
  pointer: LeaseObjectPointer,
): LeaseObjectPointer {
  const pointerKind: string = pointer.kind;
  assertAllowedKeys(
    pointer,
    new Set([
      "objectKey",
      "kind",
      "byteLength",
      "hash",
      "keyEpoch",
      "contentId",
    ]),
    "lease object pointer",
  );
  if (
    !/^packs_pk_[a-f0-9]{16,80}$/u.test(pointer.objectKey) ||
    pointerKind !== "overlay-pack"
  ) {
    throw new Error(
      "lease object pointer objectKey must be an overlay pack key",
    );
  }
  assertSafeInteger(pointer.byteLength, "lease object pointer byteLength");
  assertSafeInteger(pointer.keyEpoch, "lease object pointer keyEpoch");
  if (pointer.keyEpoch < 1) {
    throw new Error("lease object pointer keyEpoch must be positive");
  }
  if (!/^b3_[a-f0-9]{64}$/u.test(pointer.hash)) {
    throw new Error(
      "lease object pointer hash must be a b3_ BLAKE3 hex digest",
    );
  }
  return pointer;
}

function assertAllowedKeys(
  value: object,
  allowedKeys: ReadonlySet<string>,
  label: string,
): void {
  const unexpectedKeys = Object.keys(value).filter(
    (key) => !allowedKeys.has(key),
  );
  if (unexpectedKeys.length === 0) return;
  throw new Error(
    `${label} contains unsupported metadata field: ${unexpectedKeys.join(", ")}`,
  );
}

function assertSafeInteger(value: number, label: string): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${label} must be a non-negative safe integer`);
  }
}

function assertCompactLeaseDocument(label: string, value: unknown): void {
  const byteLength = Buffer.byteLength(JSON.stringify(value), "utf8");
  if (byteLength > 8_192) {
    throw new Error(
      `${label} is too large for compact control-plane metadata: ${byteLength} bytes`,
    );
  }
}

function pendingDeviceKey(mapping: PendingDeviceAccountMapping): string {
  return `${mapping.pendingDevice.workspaceId}\0${mapping.pendingDevice.accountId}\0${mapping.pendingDevice.deviceId}`;
}

function pendingDeviceMappingMatches(
  left: PendingDeviceAccountMapping,
  right: PendingDeviceAccountMapping,
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function createSequentialEventIdGenerator(): () => EventId {
  let nextId = 1;
  return () => {
    const eventId = `event_${String(nextId).padStart(6, "0")}` as EventId;
    nextId += 1;
    return eventId;
  };
}
