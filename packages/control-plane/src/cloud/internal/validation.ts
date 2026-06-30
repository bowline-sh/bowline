import { Buffer } from "node:buffer";

import type {
  ByteRange,
  DownloadIntentMetadata,
  DownloadIntentMetadataInput,
  ObjectKey,
  ObjectKind,
  ObjectMetadata,
  ObjectMetadataInput,
  PendingDeviceAccountInput,
  PendingDeviceAccountMapping,
  RetentionState,
  UploadIntentMetadata,
  UploadIntentMetadataInput,
  UpdateWorkViewInput,
  WorkViewEventKind,
  WorkViewId,
  WorkViewLifecycleState,
  WorkViewMetadata,
  WorkViewMetadataInput,
} from "../types";

export const MAX_COMPACT_DOCUMENT_BYTES = 8_192;

type ValidatedWorkViewUpdate = {
  readonly workspaceId: UpdateWorkViewInput["workspaceId"];
  readonly workViewId: WorkViewId;
  readonly expectedVersion: number;
  readonly updatedByDeviceId: UpdateWorkViewInput["updatedByDeviceId"];
  readonly updatedAt?: string;
  readonly lifecycleState?: WorkViewLifecycleState;
  readonly eventKind?: WorkViewEventKind;
  readonly overlayObjectKey?: ObjectKey;
  readonly overlayManifestId?: UpdateWorkViewInput["overlayManifestId"];
  readonly expiresAt?: string;
  readonly retainUntil?: string;
  readonly reviewReadyAt?: string;
};

const OBJECT_METADATA_KEYS = new Set([
  "workspaceId",
  "objectKey",
  "kind",
  "byteLength",
  "hash",
  "keyEpoch",
  "createdAt",
  "retentionState",
  "contentId",
  "packId",
  "manifestId",
  "createdByDeviceId",
]);

const UPLOAD_INTENT_KEYS = new Set([
  "intentId",
  "workspaceId",
  "objectKey",
  "kind",
  "byteLength",
  "expiresAt",
  "createdAt",
  "createdByDeviceId",
]);

const DOWNLOAD_INTENT_KEYS = new Set([
  "intentId",
  "workspaceId",
  "objectKey",
  "expiresAt",
  "createdAt",
  "requestedByDeviceId",
  "range",
]);

const WORK_VIEW_METADATA_KEYS = new Set([
  "workspaceId",
  "workViewId",
  "projectId",
  "name",
  "visiblePath",
  "baseSnapshotId",
  "createdAt",
  "createdByDeviceId",
  "lifecycleState",
  "overlayObjectKey",
  "overlayManifestId",
  "expiresAt",
  "retainUntil",
]);

const WORK_VIEW_UPDATE_KEYS = new Set([
  "workspaceId",
  "workViewId",
  "expectedVersion",
  "updatedAt",
  "updatedByDeviceId",
  "lifecycleState",
  "eventKind",
  "overlayObjectKey",
  "overlayManifestId",
  "expiresAt",
  "retainUntil",
  "reviewReadyAt",
]);

const OBJECT_KINDS = new Set([
  "source-pack",
  "snapshot-manifest",
  "overlay-pack",
  "index-pack",
  "locator-index",
]);
const RETENTION_STATES = new Set([
  "pending",
  "current",
  "orphan-candidate",
  "retained",
  "delete-eligible",
]);
const WORK_VIEW_LIFECYCLE_STATES = new Set([
  "active",
  "review-ready",
  "accepted",
  "discarded",
  "expired",
  "archived",
]);
const WORK_VIEW_EVENT_KINDS = new Set([
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
]);

const BLAKE3_HASH_PATTERN = /^b3_[a-f0-9]{64}$/u;
const OBJECT_KEY_PATTERNS = [
  /^packs_pk_[a-f0-9]{16,80}$/u,
  /^manifests_mf_[a-f0-9]{16,80}$/u,
  /^indexes_ix_[a-f0-9]{16,80}$/u,
];

export function validateObjectMetadata(
  input: ObjectMetadataInput,
): ObjectMetadata {
  assertAllowedKeys(input, OBJECT_METADATA_KEYS, "object metadata");

  const objectKey = validateObjectKey(input.objectKey);
  const kind = validateObjectKind(input.kind);
  assertObjectKeyMatchesKind(objectKey, kind);
  assertSafeInteger(input.byteLength, "object metadata byteLength");
  assertSafeInteger(input.keyEpoch, "object metadata keyEpoch");
  if (input.keyEpoch < 1) {
    throw new Error("object metadata keyEpoch must be positive");
  }
  assertHash(input.hash);
  const retentionState = validateRetentionState(input.retentionState);

  const metadata: ObjectMetadata = {
    byteLength: input.byteLength,
    createdAt: input.createdAt,
    hash: input.hash,
    keyEpoch: input.keyEpoch,
    kind,
    objectKey,
    retentionState,
    workspaceId: input.workspaceId,
    ...(input.contentId === undefined ? {} : { contentId: input.contentId }),
    ...(input.packId === undefined ? {} : { packId: input.packId }),
    ...(input.manifestId === undefined ? {} : { manifestId: input.manifestId }),
    ...(input.createdByDeviceId === undefined
      ? {}
      : { createdByDeviceId: input.createdByDeviceId }),
  };

  assertCompactDocument("object metadata", metadata);
  return metadata;
}

export function createUploadIntentMetadata(
  input: UploadIntentMetadataInput,
): UploadIntentMetadata {
  assertAllowedKeys(input, UPLOAD_INTENT_KEYS, "upload intent metadata");

  const objectKey = validateObjectKey(input.objectKey);
  const kind = validateObjectKind(input.kind);
  assertObjectKeyMatchesKind(objectKey, kind);
  assertSafeInteger(input.byteLength, "upload intent byteLength");

  const intent: UploadIntentMetadata = {
    byteLength: input.byteLength,
    createdAt: input.createdAt,
    createdByDeviceId: input.createdByDeviceId,
    expiresAt: input.expiresAt,
    intentId: input.intentId,
    kind,
    method: "PUT",
    objectKey,
    workspaceId: input.workspaceId,
  };

  assertCompactDocument("upload intent metadata", intent);
  return intent;
}

export function createDownloadIntentMetadata(
  input: DownloadIntentMetadataInput,
): DownloadIntentMetadata {
  assertAllowedKeys(input, DOWNLOAD_INTENT_KEYS, "download intent metadata");

  const objectKey = validateObjectKey(input.objectKey);
  const range =
    input.range === undefined ? undefined : validateByteRange(input.range);

  const intent: DownloadIntentMetadata = {
    createdAt: input.createdAt,
    expiresAt: input.expiresAt,
    intentId: input.intentId,
    method: "GET",
    objectKey,
    requestedByDeviceId: input.requestedByDeviceId,
    workspaceId: input.workspaceId,
    ...(range === undefined ? {} : { range }),
  };

  assertCompactDocument("download intent metadata", intent);
  return intent;
}

export function createPendingDeviceAccountMapping(
  input: PendingDeviceAccountInput,
): PendingDeviceAccountMapping {
  assertNoPlaintextKeyMaterial(input);

  const mapping: PendingDeviceAccountMapping = {
    account: input.account,
    pendingDevice: {
      accountId: input.account.accountId,
      decryptAuthority: "not-granted",
      deviceFingerprint: input.deviceFingerprint,
      deviceId: input.deviceId,
      deviceName: input.deviceName,
      devicePublicKey: input.devicePublicKey,
      expiresAt: input.expiresAt,
      ...(input.host === undefined ? {} : { host: input.host }),
      matchingCode: input.matchingCode,
      platform: input.platform,
      requestId: input.requestId,
      requestedAt: input.requestedAt,
      ...(input.root === undefined ? {} : { root: input.root }),
      state: "pending",
      trustState: "pending",
      workspaceId: input.workspaceId,
    },
  };

  assertNoPlaintextKeyMaterial(mapping);
  assertCompactDocument("pending device account mapping", mapping);
  return mapping;
}

export function createWorkViewMetadata(
  input: WorkViewMetadataInput,
): WorkViewMetadata {
  assertAllowedKeys(input, WORK_VIEW_METADATA_KEYS, "work view metadata");

  const workViewId = validateWorkViewId(input.workViewId);
  const lifecycleState =
    input.lifecycleState === undefined
      ? "active"
      : validateWorkViewLifecycleState(input.lifecycleState);
  const overlayObjectKey =
    input.overlayObjectKey === undefined
      ? undefined
      : validateObjectKey(input.overlayObjectKey);
  if (overlayObjectKey !== undefined) {
    assertObjectKeyMatchesKind(overlayObjectKey, "overlay-pack");
  }
  validateWorkViewName(input.name);
  validateVisibleWorkPath(input.visiblePath);

  const metadata: WorkViewMetadata = {
    baseSnapshotId: input.baseSnapshotId,
    createdAt: input.createdAt,
    createdByDeviceId: input.createdByDeviceId,
    lifecycleState,
    name: input.name,
    projectId: validateCompactIdentifier(input.projectId, "projectId"),
    updatedAt: input.createdAt,
    updatedByDeviceId: input.createdByDeviceId,
    version: 0,
    visiblePath: input.visiblePath,
    workViewId,
    workspaceId: input.workspaceId,
    ...(overlayObjectKey === undefined ? {} : { overlayObjectKey }),
    ...(input.overlayManifestId === undefined
      ? {}
      : { overlayManifestId: input.overlayManifestId }),
    ...(input.expiresAt === undefined ? {} : { expiresAt: input.expiresAt }),
    ...(input.retainUntil === undefined
      ? {}
      : { retainUntil: input.retainUntil }),
  };

  assertNoPlaintextKeyMaterial(metadata);
  assertCompactDocument("work view metadata", metadata);
  return metadata;
}

export function validateWorkViewUpdateInput(
  input: UpdateWorkViewInput,
): ValidatedWorkViewUpdate {
  assertAllowedKeys(input, WORK_VIEW_UPDATE_KEYS, "work view update");
  assertSafeInteger(input.expectedVersion, "work view expectedVersion");

  const workViewId = validateWorkViewId(input.workViewId);
  const lifecycleState =
    input.lifecycleState === undefined
      ? undefined
      : validateWorkViewLifecycleState(input.lifecycleState);
  const eventKind =
    input.eventKind === undefined
      ? undefined
      : validateWorkViewEventKind(input.eventKind);
  const overlayObjectKey =
    input.overlayObjectKey === undefined
      ? undefined
      : validateObjectKey(input.overlayObjectKey);
  if (overlayObjectKey !== undefined) {
    assertObjectKeyMatchesKind(overlayObjectKey, "overlay-pack");
  }

  const update: ValidatedWorkViewUpdate = {
    expectedVersion: input.expectedVersion,
    updatedByDeviceId: input.updatedByDeviceId,
    workViewId,
    workspaceId: input.workspaceId,
    ...(eventKind === undefined ? {} : { eventKind }),
    ...(input.expiresAt === undefined ? {} : { expiresAt: input.expiresAt }),
    ...(lifecycleState === undefined ? {} : { lifecycleState }),
    ...(input.overlayManifestId === undefined
      ? {}
      : { overlayManifestId: input.overlayManifestId }),
    ...(overlayObjectKey === undefined ? {} : { overlayObjectKey }),
    ...(input.retainUntil === undefined
      ? {}
      : { retainUntil: input.retainUntil }),
    ...(input.reviewReadyAt === undefined
      ? {}
      : { reviewReadyAt: input.reviewReadyAt }),
    ...(input.updatedAt === undefined ? {} : { updatedAt: input.updatedAt }),
  };

  assertNoPlaintextKeyMaterial(update);
  assertCompactDocument("work view update", update);
  return update;
}

export function assertCompactDocument(label: string, value: unknown): void {
  const byteLength = Buffer.byteLength(JSON.stringify(value), "utf8");
  if (byteLength > MAX_COMPACT_DOCUMENT_BYTES) {
    throw new Error(
      `${label} is too large for compact control-plane metadata: ${byteLength} bytes`,
    );
  }
}

export function assertNoPlaintextKeyMaterial(value: unknown): void {
  inspectForPlaintextKeys(value, []);
}

function validateObjectKey(value: string): ObjectKey {
  if (value.length === 0) {
    throw new Error("object metadata objectKey must not be empty");
  }
  if (value.length > 180) {
    throw new Error("object metadata objectKey is too long");
  }
  if (value.includes("/") || value.includes("\\") || value.includes(".")) {
    throw new Error("object metadata objectKey must be generated and pathless");
  }
  if (!/^[A-Za-z0-9_-]+$/u.test(value)) {
    throw new Error(
      "object metadata objectKey may contain only ASCII letters, numbers, dash, and underscore",
    );
  }
  if (!OBJECT_KEY_PATTERNS.some((pattern) => pattern.test(value))) {
    throw new Error(
      "object metadata objectKey must be an opaque generated pack, manifest, or index key",
    );
  }

  return value as ObjectKey;
}

function validateObjectKind(value: string): ObjectKind {
  if (!OBJECT_KINDS.has(value)) {
    throw new Error(`unsupported object metadata kind: ${value}`);
  }

  return value as ObjectKind;
}

function validateRetentionState(value: string): RetentionState {
  if (!RETENTION_STATES.has(value)) {
    throw new Error(`unsupported object metadata retentionState: ${value}`);
  }

  return value as RetentionState;
}

function assertObjectKeyMatchesKind(
  objectKey: ObjectKey,
  kind: ObjectKind,
): void {
  if (
    (kind === "source-pack" || kind === "overlay-pack") &&
    !objectKey.startsWith("packs_pk_")
  ) {
    throw new Error(`${kind} metadata must use a generated pack object key`);
  }
  if (
    (kind === "index-pack" || kind === "locator-index") &&
    !objectKey.startsWith("indexes_ix_")
  ) {
    throw new Error(`${kind} metadata must use a generated index object key`);
  }
  if (kind === "snapshot-manifest" && !objectKey.startsWith("manifests_mf_")) {
    throw new Error(
      "snapshot-manifest metadata must use a generated manifest object key",
    );
  }
}

function validateByteRange(range: ByteRange): ByteRange {
  assertAllowedKeys(range, new Set(["offset", "length"]), "download range");
  assertSafeInteger(range.offset, "download range offset");
  assertSafeInteger(range.length, "download range length");
  if (range.length === 0) {
    throw new Error("download range length must be greater than zero");
  }

  return { length: range.length, offset: range.offset };
}

function validateWorkViewId(value: string): WorkViewId {
  return validateCompactIdentifier(value, "workViewId") as WorkViewId;
}

function validateCompactIdentifier(value: string, label: string): string {
  if (value.length === 0) {
    throw new Error(`work view ${label} must not be empty`);
  }
  if (value.length > 160) {
    throw new Error(`work view ${label} is too long`);
  }
  if (value.includes("/") || value.includes("\\") || value.includes(".")) {
    throw new Error(`work view ${label} must be pathless`);
  }
  if (!/^[A-Za-z0-9:_-]+$/u.test(value)) {
    throw new Error(
      `work view ${label} may contain only ASCII letters, numbers, colon, dash, and underscore`,
    );
  }
  return value;
}

function validateWorkViewName(value: string): void {
  if (
    value.length === 0 ||
    value === "." ||
    value === ".." ||
    value.includes("/") ||
    value.includes("\\") ||
    value.includes("//")
  ) {
    throw new Error("work view name must be a single safe path segment");
  }
}

function validateVisibleWorkPath(value: string): void {
  const segments = value.split("/");
  if (
    value.length === 0 ||
    !value.startsWith(".work/") ||
    value.startsWith("/") ||
    value.startsWith("~/") ||
    value.includes("\0") ||
    value.includes("/../") ||
    value.endsWith("/..") ||
    segments.some((segment) => segment.length === 0 || segment === ".")
  ) {
    throw new Error("work view visiblePath must be a relative .work path");
  }
}

function validateWorkViewLifecycleState(value: string): WorkViewLifecycleState {
  if (!WORK_VIEW_LIFECYCLE_STATES.has(value)) {
    throw new Error(`unsupported work view lifecycleState: ${value}`);
  }
  return value as WorkViewLifecycleState;
}

function validateWorkViewEventKind(value: string): WorkViewEventKind {
  if (!WORK_VIEW_EVENT_KINDS.has(value)) {
    throw new Error(`unsupported work view event kind: ${value}`);
  }
  return value as WorkViewEventKind;
}

function assertSafeInteger(value: number, label: string): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${label} must be a non-negative safe integer`);
  }
}

function assertHash(value: string): void {
  if (!BLAKE3_HASH_PATTERN.test(value)) {
    throw new Error("object metadata hash must be a b3_ BLAKE3 hex digest");
  }
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

function inspectForPlaintextKeys(
  value: unknown,
  path: readonly string[],
): void {
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      inspectForPlaintextKeys(item, [...path, String(index)]),
    );
    return;
  }

  if (!isRecord(value)) return;

  for (const [key, childValue] of Object.entries(value)) {
    if (isPlaintextKeyField(key)) {
      throw new Error(
        `plaintext key material is not allowed in control-plane metadata: ${[
          ...path,
          key,
        ].join(".")}`,
      );
    }
    inspectForPlaintextKeys(childValue, [...path, key]);
  }
}

function isPlaintextKeyField(key: string): boolean {
  const normalized = key.toLowerCase().replace(/[^a-z0-9]/gu, "");
  return (
    normalized.includes("plaintext") ||
    normalized.includes("workspacekey") ||
    normalized.includes("decryptkey") ||
    normalized.includes("privatekey") ||
    normalized.includes("secretkey") ||
    normalized.includes("recoverykey")
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
