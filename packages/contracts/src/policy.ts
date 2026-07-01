export const PATH_CLASSIFICATIONS = [
  "workspace-sync",
  "project-env",
  "generated",
  "dependency",
  "cache",
  "large-file",
  "secret-looking",
  "local-only",
  "blocked",
] as const;
export type PathClassification = (typeof PATH_CLASSIFICATIONS)[number];

export const MATERIALIZATION_MODES = [
  "workspace-sync",
  "project-env",
  "encrypted-sync",
  "lazy",
  "structure-only",
  "local-regenerate",
  "local-cache",
  "ignore",
  "local-only",
  "blocked",
] as const;
export type MaterializationMode = (typeof MATERIALIZATION_MODES)[number];

export const ACCESS_FLAGS = [
  "human-readable",
  "agent-readable",
  "agent-hidden",
  "lease-only",
] as const;
export type AccessFlag = (typeof ACCESS_FLAGS)[number];
