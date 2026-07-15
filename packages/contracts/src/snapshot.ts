import type {
  Brand,
  ContentId,
  ManifestDigest,
  NamespacePageId,
  PackId,
  ProjectId,
  SnapshotId,
  WorkspaceId,
} from "./ids";
import type { SNAPSHOT_SCHEMA_VERSION } from "./ids";
import type {
  AccessFlag,
  MaterializationMode,
  PathClassification,
} from "./policy";

export const SNAPSHOT_KINDS = [
  "base",
  "machine",
  "workspace-head",
  "agent-overlay",
  "conflict",
] as const;
export type SnapshotKind = (typeof SNAPSHOT_KINDS)[number];

export const REF_KINDS = ["workspace", "machine", "project", "lease"] as const;
export type RefKind = (typeof REF_KINDS)[number];

export const NAMESPACE_ENTRY_KINDS = [
  "directory",
  "file",
  "symlink",
  "placeholder",
  "tombstone",
] as const;
export type NamespaceEntryKind = (typeof NAMESPACE_ENTRY_KINDS)[number];

export const HYDRATION_STATES = [
  "local",
  "cold",
  "structure-only",
  "missing",
] as const;
export type HydrationState = (typeof HYDRATION_STATES)[number];

export const FILE_EXECUTABILITIES = ["regular", "executable"] as const;
export type FileExecutability = (typeof FILE_EXECUTABILITIES)[number];

export const CONTENT_STORAGES = ["inline", "packed"] as const;
export type ContentStorage = (typeof CONTENT_STORAGES)[number];

export type ContentLocator = {
  readonly contentId: ContentId;
  readonly storage: ContentStorage;
  readonly rawSize: number;
  readonly packId?: PackId;
  readonly offset?: number;
  readonly length?: number;
};

export type SegmentId = Brand<"SegmentId">;

export type SegmentLocator = {
  readonly ordinal: number;
  readonly plaintextLength: number;
  readonly segmentId: SegmentId;
  readonly packId: PackId;
  readonly offset: number;
  readonly length: number;
  readonly formatVersion: number;
};

/**
 * Physical representation of one logical file. `NamespaceEntry.contentId`
 * remains the whole-file identity while segments describe physical storage.
 */
export type ContentLayout = {
  readonly kind: "segmented-v1";
  readonly logicalContentId: ContentId;
  readonly logicalLength: number;
  readonly segmentSize: number;
  readonly segments: readonly SegmentLocator[];
};

export type NamespaceEntry = {
  readonly path: string;
  readonly kind: NamespaceEntryKind;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access?: readonly AccessFlag[];
  readonly contentId?: ContentId;
  readonly contentLayout?: ContentLayout;
  readonly symlinkTarget?: string;
  readonly byteLen?: number;
  readonly executability?: FileExecutability;
  readonly hydrationState: HydrationState;
};

export type WorkspaceRef = {
  readonly name: string;
  readonly targetSnapshotId: SnapshotId;
  readonly kind: RefKind;
};

export type SnapshotManifest = {
  readonly schemaVersion: typeof SNAPSHOT_SCHEMA_VERSION;
  readonly snapshotId: SnapshotId;
  readonly workspaceId: WorkspaceId;
  readonly projectId?: ProjectId;
  readonly kind: SnapshotKind;
  readonly baseSnapshotId?: SnapshotId;
  readonly namespaceRootId: NamespacePageId;
  readonly semanticManifestDigest: ManifestDigest;
  readonly entryCount: number;
  readonly refs: readonly WorkspaceRef[];
};
