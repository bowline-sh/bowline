export type Brand<TName extends string> = string & { readonly __brand: TName };

export type WorkspaceId = Brand<"WorkspaceId">;
export type AccountId = Brand<"AccountId">;
export type DeviceId = Brand<"DeviceId">;
export type DeviceApprovalRequestId = Brand<"DeviceApprovalRequestId">;
export type EncryptedDeviceGrantId = Brand<"EncryptedDeviceGrantId">;
export type RecoveryEnvelopeId = Brand<"RecoveryEnvelopeId">;
export type WorkOsUserId = Brand<"WorkOsUserId">;
export type WorkOsOrganizationId = Brand<"WorkOsOrganizationId">;
export type ProjectId = Brand<"ProjectId">;
export type SnapshotId = Brand<"SnapshotId">;
export type ManifestId = Brand<"ManifestId">;
export type PackId = Brand<"PackId">;
export type ContentId = Brand<"ContentId">;
export type LeaseId = Brand<"LeaseId">;
export type WorkViewId = Brand<"WorkViewId">;
export type EventId = Brand<"EventId">;
export type PolicyVersion = Brand<"PolicyVersion">;
export type EnvRecordId = Brand<"EnvRecordId">;

export const CONTRACT_VERSION = 3;

export const SCHEMA_SOURCE_OF_TRUTH = "hand-written-fixtures";
