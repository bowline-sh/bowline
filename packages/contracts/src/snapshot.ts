import type { ContentId, PackId, ProjectId, SnapshotId, WorkspaceId } from "./ids";
import type { CONTRACT_VERSION } from "./ids";
import type { AccessFlag, MaterializationMode, PathClassification } from "./policy";

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

export const CONTENT_STORAGES = ["inline", "packed", "chunked"] as const;
export type ContentStorage = (typeof CONTENT_STORAGES)[number];

export type ContentLocator = {
  readonly contentId: ContentId;
  readonly storage: ContentStorage;
  readonly rawSize: number;
  readonly packId?: PackId;
  readonly offset?: number;
  readonly length?: number;
  readonly chunkIds?: readonly ContentId[];
};

export type NamespaceEntry = {
  readonly path: string;
  readonly kind: NamespaceEntryKind;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access?: readonly AccessFlag[];
  readonly contentId?: ContentId;
  readonly locator?: ContentLocator;
  readonly symlinkTarget?: string;
  readonly byteLen?: number;
  readonly hydrationState: HydrationState;
};

export type WorkspaceRef = {
  readonly name: string;
  readonly targetSnapshotId: SnapshotId;
  readonly kind: RefKind;
};

export type SnapshotManifest = {
  readonly schemaVersion: typeof CONTRACT_VERSION;
  readonly snapshotId: SnapshotId;
  readonly workspaceId: WorkspaceId;
  readonly projectId?: ProjectId;
  readonly kind: SnapshotKind;
  readonly baseSnapshotId?: SnapshotId;
  readonly entries: readonly NamespaceEntry[];
  readonly refs: readonly WorkspaceRef[];
};
