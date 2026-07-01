import type {
  Brand,
  DeviceApprovalRequestId,
  DeviceId,
  EncryptedDeviceGrantId,
  RecoveryEnvelopeId,
  WorkspaceId,
} from "./ids";

export type DeviceFingerprint = Brand<"DeviceFingerprint">;
export type PublicDeviceKey = Brand<"PublicDeviceKey">;

export const DEVICE_APPROVAL_REQUEST_STATES = [
  "pending",
  "approved",
  "denied",
  "expired",
] as const;
export type DeviceApprovalRequestState =
  (typeof DEVICE_APPROVAL_REQUEST_STATES)[number];

export type DeviceApprovalRequest = {
  readonly requestId: DeviceApprovalRequestId;
  readonly workspaceId: WorkspaceId;
  readonly requesterDeviceId: DeviceId;
  readonly deviceName: string;
  readonly platform: DevicePlatform;
  readonly devicePublicKey: PublicDeviceKey;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly matchingCode: string;
  readonly requestedAt: string;
  readonly expiresAt: string;
  readonly state: DeviceApprovalRequestState;
  readonly host?: string;
  readonly root?: string;
};

export type DevicePlatform = "macos" | "linux" | "unknown";

export type AuthorizedDevice = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly authorizedAt: string;
  readonly authorizedByDeviceId?: DeviceId;
};

export type DeviceTrustState =
  | "trusted"
  | "pending"
  | "revoked"
  | "limited"
  | "unavailable"
  | "first-device-setup";

export type DeviceRecord = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly trustState: DeviceTrustState;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly authorizedAt?: string;
  readonly updatedAt: string;
  readonly isCurrentDevice: boolean;
  readonly limitationReason?: string;
};

export type RevokedDevice = {
  readonly id: DeviceId;
  readonly name: string;
  readonly workspaceId: WorkspaceId;
  readonly platform: DevicePlatform;
  readonly deviceFingerprint: DeviceFingerprint;
  readonly revokedAt: string;
  readonly revokedByDeviceId: DeviceId;
  readonly reason: string;
};

export type EncryptedDeviceGrantState =
  | "created"
  | "accepted"
  | "expired"
  | "revoked";

export type RecoveryGrantApproverId = Brand<"RecoveryGrantApproverId">;
export type DeviceGrantApproverId = DeviceId | RecoveryGrantApproverId;

export type EncryptedDeviceGrant = {
  readonly grantId: EncryptedDeviceGrantId;
  readonly requestId: DeviceApprovalRequestId;
  readonly workspaceId: WorkspaceId;
  readonly requesterDeviceId: DeviceId;
  readonly requesterDeviceFingerprint: DeviceFingerprint;
  readonly approverDeviceId: DeviceGrantApproverId;
  readonly keyEpoch: number;
  readonly ciphertext: string;
  readonly createdAt: string;
  readonly expiresAt: string;
  readonly state: EncryptedDeviceGrantState;
  readonly acceptedAt?: string;
};

export type RecoveryKeyLifecycle =
  | "missing"
  | "generated-unverified"
  | "active"
  | "rotated"
  | "revoked";

export type RecoveryKeyState = {
  readonly lifecycle: RecoveryKeyLifecycle;
  readonly envelopeId?: RecoveryEnvelopeId;
  readonly fingerprint?: string;
  readonly createdAt?: string;
  readonly verifiedAt?: string;
  readonly rotatedAt?: string;
  readonly revokedAt?: string;
};
