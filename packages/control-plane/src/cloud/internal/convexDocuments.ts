import type {
  CompactEvent,
  ObjectMetadata,
  PendingDeviceAccountMapping,
  WorkspaceRef,
} from "../types";

export const CONVEX_TABLES = {
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
} as const;

export const CONVEX_FUNCTIONS = {
  compareAndSwapWorkspaceRef: "refs:compareAndSwapWorkspaceRef",
  createDownloadIntent: "objects:createDownloadIntent",
  createFirstAuthorizedDevice: "devices:createFirstAuthorizedDevice",
  createPendingDevice: "devices:createPendingDevice",
  createRecoveryEnvelope: "recovery:createRecoveryEnvelope",
  createUploadIntent: "objects:createUploadIntent",
  listDeviceTrust: "devices:listDeviceTrust",
  listCompactEvents: "events:listCompactEvents",
} as const;

export type ConvexWorkspaceRefDocument = {
  readonly workspaceId: string;
  readonly version: number;
  readonly snapshotId: string;
  readonly updatedAt: string;
  readonly updatedByDeviceId?: string;
};

export type ConvexCompactEventDocument = {
  readonly eventId: string;
  readonly workspaceId: string;
  readonly occurredAt: string;
  readonly kind: string;
  readonly subject: string;
};

export type ConvexObjectMetadataDocument = {
  readonly workspaceId: string;
  readonly objectKey: string;
  readonly kind: string;
  readonly byteLength: number;
  readonly hash: string;
  readonly keyEpoch: number;
  readonly createdAt: string;
  readonly retentionState: string;
  readonly contentId?: string;
  readonly packId?: string;
  readonly manifestId?: string;
  readonly createdByDeviceId?: string;
};

export type ConvexPendingDeviceDocument = {
  readonly accountId: string;
  readonly workOsUserId: string;
  readonly workOsOrganizationId?: string;
  readonly workspaceId: string;
  readonly requestId: string;
  readonly deviceId: string;
  readonly deviceName: string;
  readonly devicePublicKey: string;
  readonly deviceFingerprint: string;
  readonly platform: string;
  readonly matchingCode: string;
  readonly requestedAt: string;
  readonly expiresAt: string;
  readonly state: "pending";
  readonly trustState: "pending";
  readonly decryptAuthority: "not-granted";
  readonly host?: string;
  readonly root?: string;
};

export function toConvexWorkspaceRefDocument(
  ref: WorkspaceRef,
): ConvexWorkspaceRefDocument {
  return {
    snapshotId: ref.snapshotId,
    updatedAt: ref.updatedAt,
    version: ref.version,
    workspaceId: ref.workspaceId,
    ...(ref.updatedByDeviceId === undefined
      ? {}
      : { updatedByDeviceId: ref.updatedByDeviceId }),
  };
}

export function toConvexCompactEventDocument(
  event: CompactEvent,
): ConvexCompactEventDocument {
  return {
    eventId: event.eventId,
    kind: event.kind,
    occurredAt: event.occurredAt,
    subject: event.subject,
    workspaceId: event.workspaceId,
  };
}

export function toConvexObjectMetadataDocument(
  metadata: ObjectMetadata,
): ConvexObjectMetadataDocument {
  return {
    byteLength: metadata.byteLength,
    createdAt: metadata.createdAt,
    hash: metadata.hash,
    keyEpoch: metadata.keyEpoch,
    kind: metadata.kind,
    objectKey: metadata.objectKey,
    retentionState: metadata.retentionState,
    workspaceId: metadata.workspaceId,
    ...(metadata.contentId === undefined
      ? {}
      : { contentId: metadata.contentId }),
    ...(metadata.packId === undefined ? {} : { packId: metadata.packId }),
    ...(metadata.manifestId === undefined
      ? {}
      : { manifestId: metadata.manifestId }),
    ...(metadata.createdByDeviceId === undefined
      ? {}
      : { createdByDeviceId: metadata.createdByDeviceId }),
  };
}

export function toConvexPendingDeviceDocument(
  mapping: PendingDeviceAccountMapping,
): ConvexPendingDeviceDocument {
  return {
    accountId: mapping.pendingDevice.accountId,
    decryptAuthority: mapping.pendingDevice.decryptAuthority,
    deviceFingerprint: mapping.pendingDevice.deviceFingerprint,
    deviceId: mapping.pendingDevice.deviceId,
    deviceName: mapping.pendingDevice.deviceName,
    devicePublicKey: mapping.pendingDevice.devicePublicKey,
    expiresAt: mapping.pendingDevice.expiresAt,
    ...(mapping.pendingDevice.host === undefined
      ? {}
      : { host: mapping.pendingDevice.host }),
    matchingCode: mapping.pendingDevice.matchingCode,
    platform: mapping.pendingDevice.platform,
    requestId: mapping.pendingDevice.requestId,
    requestedAt: mapping.pendingDevice.requestedAt,
    ...(mapping.pendingDevice.root === undefined
      ? {}
      : { root: mapping.pendingDevice.root }),
    state: mapping.pendingDevice.state,
    trustState: mapping.pendingDevice.trustState,
    workOsOrganizationId: mapping.account.workOsOrganizationId,
    workOsUserId: mapping.account.workOsUserId,
    workspaceId: mapping.pendingDevice.workspaceId,
  };
}
