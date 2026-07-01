import type {
  ContentId,
  DeviceId,
  EventId,
  ManifestId,
  PackId,
  SnapshotId,
  WorkspaceId,
} from "@bowline/contracts/ids";
import type { DevicePlatform } from "@bowline/contracts/devices";

type Brand<TName extends string> = string & { readonly __brand: TName };

export type AccountId = Brand<"AccountId">;
export type IntentId = Brand<"IntentId">;
export type ObjectKey = Brand<"ObjectKey">;
export type WorkViewId = Brand<"WorkViewId">;
export type WorkOsOrganizationId = Brand<"WorkOsOrganizationId">;
export type WorkOsUserId = Brand<"WorkOsUserId">;

export type WorkspaceRef = {
  readonly workspaceId: WorkspaceId;
  readonly version: number;
  readonly snapshotId: SnapshotId;
  readonly updatedAt: string;
  readonly updatedByDeviceId?: DeviceId;
};

export type CompactEventKind =
  | "workspace.created"
  | "workspace_ref.advanced"
  | "object_pointer.added"
  | "object_manifest.committed"
  | "device.requested"
  | "device.harness_approved"
  | "device.approval_requested"
  | "device.approved"
  | "device.denied"
  | "device.revoked"
  | "recovery_key.created"
  | "recovery_key.verified"
  | "recovery_key.rotated"
  | "recovery_key.revoked"
  | "auth.login_started"
  | "auth.login_completed"
  | "lease.created"
  | "lease.updated"
  | "lease.expired"
  | "lease.completed"
  | "lease.blocked"
  | "lease.revoked"
  | "lease.review_ready"
  | "lease.tool_invoked"
  | "lease.tool_denied"
  | "lease.hydration_requested"
  | "overlay.changed"
  | "publish.requested"
  | "lease.cleanup_completed"
  | WorkViewEventKind;

export type WorkViewEventKind =
  | "work.created"
  | "work.updated"
  | "work.review_ready"
  | "work.accepted"
  | "work.discarded"
  | "work.restored"
  | "work.expired"
  | "work.archived"
  | "work.cleanup_previewed"
  | "work.cleanup_completed";

export type CompactEvent = {
  readonly eventId: EventId;
  readonly workspaceId: WorkspaceId;
  readonly occurredAt: string;
  readonly kind: CompactEventKind;
  readonly subject: string;
};

export type ObjectKind =
  | "source-pack"
  | "snapshot-manifest"
  | "overlay-pack"
  | "index-pack"
  | "locator-index";

export type RetentionState =
  | "pending"
  | "current"
  | "orphan-candidate"
  | "retained"
  | "delete-eligible";

export type ObjectMetadataInput = {
  readonly workspaceId: WorkspaceId;
  readonly objectKey: string;
  readonly kind: ObjectKind;
  readonly byteLength: number;
  readonly hash: string;
  readonly keyEpoch: number;
  readonly createdAt: string;
  readonly retentionState: RetentionState;
  readonly contentId?: ContentId;
  readonly packId?: PackId;
  readonly manifestId?: ManifestId;
  readonly createdByDeviceId?: DeviceId;
};

export type ObjectMetadata = {
  readonly workspaceId: WorkspaceId;
  readonly objectKey: ObjectKey;
  readonly kind: ObjectKind;
  readonly byteLength: number;
  readonly hash: string;
  readonly keyEpoch: number;
  readonly createdAt: string;
  readonly retentionState: RetentionState;
  readonly contentId?: ContentId;
  readonly packId?: PackId;
  readonly manifestId?: ManifestId;
  readonly createdByDeviceId?: DeviceId;
};

export type ByteRange = {
  readonly offset: number;
  readonly length: number;
};

export type UploadIntentMetadataInput = {
  readonly intentId: IntentId;
  readonly workspaceId: WorkspaceId;
  readonly objectKey: string;
  readonly kind: ObjectKind;
  readonly byteLength: number;
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly createdByDeviceId: DeviceId;
};

export type UploadIntentMetadata = {
  readonly intentId: IntentId;
  readonly workspaceId: WorkspaceId;
  readonly objectKey: ObjectKey;
  readonly kind: ObjectKind;
  readonly byteLength: number;
  readonly method: "PUT";
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly createdByDeviceId: DeviceId;
};

export type DownloadIntentMetadataInput = {
  readonly intentId: IntentId;
  readonly workspaceId: WorkspaceId;
  readonly objectKey: string;
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly requestedByDeviceId: DeviceId;
  readonly range?: ByteRange;
};

export type DownloadIntentMetadata = {
  readonly intentId: IntentId;
  readonly workspaceId: WorkspaceId;
  readonly objectKey: ObjectKey;
  readonly method: "GET";
  readonly expiresAt: string;
  readonly createdAt: string;
  readonly requestedByDeviceId: DeviceId;
  readonly range?: ByteRange;
};

export type WorkOsAccountIdentity = {
  readonly accountId: AccountId;
  readonly workOsUserId: WorkOsUserId;
  readonly workOsOrganizationId: WorkOsOrganizationId;
  readonly email?: string;
};

export type PendingDeviceAccountInput = {
  readonly account: WorkOsAccountIdentity;
  readonly workspaceId: WorkspaceId;
  readonly deviceId: DeviceId;
  readonly deviceName: string;
  readonly devicePublicKey: string;
  readonly deviceFingerprint: string;
  readonly platform: DevicePlatform;
  readonly matchingCode: string;
  readonly requestedAt: string;
  readonly expiresAt: string;
  readonly requestId: string;
  readonly host?: string;
  readonly root?: string;
};

export type PendingDeviceAccountMapping = {
  readonly account: WorkOsAccountIdentity;
  readonly pendingDevice: {
    readonly accountId: AccountId;
    readonly workspaceId: WorkspaceId;
    readonly requestId: string;
    readonly deviceId: DeviceId;
    readonly deviceName: string;
    readonly devicePublicKey: string;
    readonly deviceFingerprint: string;
    readonly platform: DevicePlatform;
    readonly matchingCode: string;
    readonly requestedAt: string;
    readonly expiresAt: string;
    readonly state: "pending";
    readonly trustState: "pending";
    readonly decryptAuthority: "not-granted";
    readonly host?: string;
    readonly root?: string;
  };
};

export type CreateWorkspaceRefInput = {
  readonly workspaceId: WorkspaceId;
  readonly snapshotId: SnapshotId;
  readonly createdAt?: string;
  readonly createdByDeviceId?: DeviceId;
};

export type CompareAndSwapWorkspaceRefInput = {
  readonly workspaceId: WorkspaceId;
  readonly expectedVersion: number;
  readonly nextSnapshotId: SnapshotId;
  readonly writerDeviceId: DeviceId;
  readonly updatedAt?: string;
};

export type CompareAndSwapWorkspaceRefResult =
  | {
      readonly ok: true;
      readonly ref: WorkspaceRef;
      readonly event: CompactEvent;
    }
  | {
      readonly ok: false;
      readonly error: "workspace-missing";
      readonly workspaceId: WorkspaceId;
    }
  | {
      readonly ok: false;
      readonly error: "stale-ref";
      readonly currentRef: WorkspaceRef;
    };

export type WorkViewLifecycleState =
  | "active"
  | "review-ready"
  | "accepted"
  | "discarded"
  | "expired"
  | "archived";

export type WorkViewMetadataInput = {
  readonly workspaceId: WorkspaceId;
  readonly workViewId: string;
  readonly projectId: string;
  readonly name: string;
  readonly visiblePath: string;
  readonly baseSnapshotId: SnapshotId;
  readonly createdAt: string;
  readonly createdByDeviceId: DeviceId;
  readonly lifecycleState?: WorkViewLifecycleState;
  readonly overlayObjectKey?: string;
  readonly overlayManifestId?: ManifestId;
  readonly expiresAt?: string;
  readonly retainUntil?: string;
};

export type WorkViewMetadata = {
  readonly workspaceId: WorkspaceId;
  readonly workViewId: WorkViewId;
  readonly projectId: string;
  readonly name: string;
  readonly visiblePath: string;
  readonly baseSnapshotId: SnapshotId;
  readonly lifecycleState: WorkViewLifecycleState;
  readonly version: number;
  readonly createdAt: string;
  readonly createdByDeviceId: DeviceId;
  readonly updatedAt: string;
  readonly updatedByDeviceId: DeviceId;
  readonly overlayObjectKey?: ObjectKey;
  readonly overlayManifestId?: ManifestId;
  readonly expiresAt?: string;
  readonly retainUntil?: string;
  readonly reviewReadyAt?: string;
};

export type UpdateWorkViewInput = {
  readonly workspaceId: WorkspaceId;
  readonly workViewId: string;
  readonly expectedVersion: number;
  readonly updatedAt?: string;
  readonly updatedByDeviceId: DeviceId;
  readonly lifecycleState?: WorkViewLifecycleState;
  readonly eventKind?: WorkViewEventKind;
  readonly overlayObjectKey?: string;
  readonly overlayManifestId?: ManifestId;
  readonly expiresAt?: string;
  readonly retainUntil?: string;
  readonly reviewReadyAt?: string;
};

export type UpdateWorkViewResult =
  | {
      readonly ok: true;
      readonly workView: WorkViewMetadata;
      readonly event: CompactEvent;
    }
  | {
      readonly ok: false;
      readonly error: "work-view-missing";
      readonly workspaceId: WorkspaceId;
      readonly workViewId: string;
    }
  | {
      readonly ok: false;
      readonly error: "stale-work-view";
      readonly currentWorkView: WorkViewMetadata;
    };

export type LeaseExecutionState =
  | "active"
  | "blocked"
  | "completed"
  | "expired"
  | "revoked";

export type LeaseOutputState =
  | "empty"
  | "dirty"
  | "review-ready"
  | "accepted"
  | "discarded"
  | "conflicted"
  | "retained";

export type LeaseWriteTargetMode = "direct" | "work-view";

export type LeaseEventKind =
  | "lease.updated"
  | "lease.expired"
  | "lease.completed"
  | "lease.blocked"
  | "lease.revoked"
  | "lease.review_ready"
  | "lease.tool_invoked"
  | "lease.tool_denied"
  | "lease.hydration_requested"
  | "overlay.changed"
  | "publish.requested"
  | "lease.cleanup_completed";

export type LeaseObjectPointer = {
  readonly objectKey: string;
  readonly kind: "overlay-pack";
  readonly byteLength: number;
  readonly hash: string;
  readonly keyEpoch: number;
  readonly contentId: ContentId;
};

export type LeaseMetadataInput = {
  readonly workspaceId: WorkspaceId;
  readonly leaseId: string;
  readonly projectId: string;
  readonly deviceId: DeviceId;
  readonly writeTargetMode: LeaseWriteTargetMode;
  readonly workViewId?: string | undefined;
  readonly baseSnapshotId: SnapshotId;
  readonly createdAt: string;
  readonly expiresAt: string;
  readonly executionState?: LeaseExecutionState;
  readonly outputState?: LeaseOutputState;
  readonly statusCode: string;
  readonly outputObject?: LeaseObjectPointer;
  readonly auditObject?: LeaseObjectPointer;
};

export type LeaseMetadata = {
  readonly workspaceId: WorkspaceId;
  readonly leaseId: string;
  readonly projectId: string;
  readonly deviceId: DeviceId;
  readonly writeTargetMode: LeaseWriteTargetMode;
  readonly workViewId?: WorkViewId | undefined;
  readonly baseSnapshotId: SnapshotId;
  readonly executionState: LeaseExecutionState;
  readonly outputState: LeaseOutputState;
  readonly statusCode: string;
  readonly version: number;
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly expiresAt: string;
  readonly outputObject?: LeaseObjectPointer;
  readonly auditObject?: LeaseObjectPointer;
};

export type UpdateLeaseInput = {
  readonly workspaceId: WorkspaceId;
  readonly leaseId: string;
  readonly expectedVersion: number;
  readonly updatedAt?: string;
  readonly updatedByDeviceId: DeviceId;
  readonly executionState?: LeaseExecutionState;
  readonly outputState?: LeaseOutputState;
  readonly statusCode?: string;
  readonly outputObject?: LeaseObjectPointer;
  readonly auditObject?: LeaseObjectPointer;
  readonly eventKind?: LeaseEventKind;
};

export type UpdateLeaseResult =
  | {
      readonly ok: true;
      readonly lease: LeaseMetadata;
      readonly event: CompactEvent;
    }
  | {
      readonly ok: false;
      readonly error: "lease-missing";
      readonly workspaceId: WorkspaceId;
      readonly leaseId: string;
    }
  | {
      readonly ok: false;
      readonly error: "stale-lease";
      readonly currentLease: LeaseMetadata;
    };

export type CompactWorkspaceMetadata = {
  readonly workspaceId: WorkspaceId;
  readonly ref?: WorkspaceRef;
  readonly eventCount: number;
  readonly leaseCount: number;
  readonly objectCount: number;
  readonly pendingDeviceCount: number;
  readonly workViewCount: number;
  readonly latestEventId?: EventId;
};

export type CloudMetadataStore = {
  readonly createWorkspaceRef: (input: CreateWorkspaceRefInput) => WorkspaceRef;
  readonly getWorkspaceRef: (
    workspaceId: WorkspaceId,
  ) => WorkspaceRef | undefined;
  readonly compareAndSwapWorkspaceRef: (
    input: CompareAndSwapWorkspaceRefInput,
  ) => CompareAndSwapWorkspaceRefResult;
  readonly commitObjectMetadata: (input: ObjectMetadataInput) => CompactEvent;
  readonly listObjectMetadata: (
    workspaceId: WorkspaceId,
  ) => readonly ObjectMetadata[];
  readonly createWorkView: (input: WorkViewMetadataInput) => {
    readonly workView: WorkViewMetadata;
    readonly event: CompactEvent;
  };
  readonly updateWorkView: (input: UpdateWorkViewInput) => UpdateWorkViewResult;
  readonly listWorkViews: (
    workspaceId: WorkspaceId,
  ) => readonly WorkViewMetadata[];
  readonly createLease: (input: LeaseMetadataInput) => {
    readonly lease: LeaseMetadata;
    readonly event: CompactEvent;
  };
  readonly updateLease: (input: UpdateLeaseInput) => UpdateLeaseResult;
  readonly listLeases: (workspaceId: WorkspaceId) => readonly LeaseMetadata[];
  readonly listEvents: (workspaceId: WorkspaceId) => readonly CompactEvent[];
  readonly createPendingDeviceAccountMapping: (
    input: PendingDeviceAccountInput,
  ) => PendingDeviceAccountMapping;
  readonly getCompactWorkspaceMetadata: (
    workspaceId: WorkspaceId,
  ) => CompactWorkspaceMetadata;
};
