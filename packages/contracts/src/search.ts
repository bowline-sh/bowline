import type { ProjectId, SnapshotId } from "./ids";
import type { AccessFlag, MaterializationMode, PathClassification } from "./policy";
import type { HydrationBudgetStatus, IndexStatus, SafeAction, WorkspaceStatus } from "./status";
import type { CommandOutputBase } from "./commands";
import type { HydrationState } from "./snapshot";

export type SearchResult = {
  readonly path: string;
  readonly score: number;
  readonly projectId?: ProjectId;
  readonly snapshotId?: SnapshotId;
  readonly lineStart?: number;
  readonly lineEnd?: number;
  readonly snippet?: string;
  readonly classification: PathClassification;
  readonly mode: MaterializationMode;
  readonly access: readonly AccessFlag[];
  readonly hydrationState: HydrationState;
};

export type SearchCommandOutput = CommandOutputBase<"search"> & {
  readonly query: string;
  readonly requestedPath?: string;
  readonly index: IndexStatus;
  readonly budget?: HydrationBudgetStatus;
  readonly results: readonly SearchResult[];
  readonly truncated: boolean;
  readonly nextCursor?: string;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};

export const SYMBOL_KINDS = [
  "function",
  "class",
  "method",
  "variable",
  "constant",
  "type",
  "interface",
  "module",
  "import",
  "export",
  "struct",
  "enum",
  "trait",
] as const;
export type SymbolKind = (typeof SYMBOL_KINDS)[number];

export const SYMBOL_LANGUAGES = [
  "typescript",
  "javascript",
  "python",
  "rust",
  "go",
  "unknown",
] as const;
export type SymbolLanguage = (typeof SYMBOL_LANGUAGES)[number];

export type SymbolResult = {
  readonly name: string;
  readonly kind: SymbolKind;
  readonly language: SymbolLanguage;
  readonly path: string;
  readonly lineStart: number;
  readonly lineEnd: number;
  readonly projectId?: ProjectId;
  readonly snapshotId?: SnapshotId;
  readonly container?: string;
  readonly signature?: string;
  readonly referenceCount?: number;
  readonly classification: PathClassification;
  readonly access: readonly AccessFlag[];
  readonly hydrationState: HydrationState;
};

export type SymbolCommandOutput = CommandOutputBase<"symbols"> & {
  readonly query: string;
  readonly requestedPath?: string;
  readonly index: IndexStatus;
  readonly budget?: HydrationBudgetStatus;
  readonly symbols: readonly SymbolResult[];
  readonly truncated: boolean;
  readonly nextCursor?: string;
  readonly status: WorkspaceStatus;
  readonly nextActions: readonly SafeAction[];
};
