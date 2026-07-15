import type { RepairCommand, StatusLevel } from "./status";
import type { AgentCliCapability } from "./agent";
import type { CommandOutputBase } from "./commands";

export type ResolveAction =
  | "list"
  | "copy-prompt"
  | "diff"
  | "agent"
  | "accept"
  | "reject";

export type ResolveAgent = "codex" | "claude" | "cursor";

export type ResolveConflict = {
  readonly id: string;
  readonly state: "unresolved";
  readonly bundlePath: string;
  readonly conflictKind?: string;
  readonly affectedFiles: readonly string[];
  readonly spans?: readonly ResolveConflictSpan[];
  readonly activeView: string;
  readonly hasResolutionOverlay: boolean;
  readonly containsSecrets: boolean;
};

export type ResolveConflictSpan = {
  readonly path: string;
  readonly baseStartLine: number;
  readonly baseEndLine: number;
  readonly localStartLine: number;
  readonly localEndLine: number;
  readonly remoteStartLine: number;
  readonly remoteEndLine: number;
  readonly baseContextHash?: string;
  readonly localContextHash?: string;
  readonly remoteContextHash?: string;
};

export type ResolveAgentOption = {
  readonly name: ResolveAgent;
  readonly command: string;
  readonly capability: AgentCliCapability;
};

export type ResolveAvailableAction = {
  readonly label: string;
  readonly command?: string;
};

export type ResolvePrompt = {
  readonly conflictId: string;
  readonly bundlePath: string;
  readonly resolutionPath: string;
  readonly redaction: "applied";
  readonly text: string;
};

export type ResolveDiff = {
  readonly conflictId: string;
  readonly bundlePath: string;
  readonly redaction: "contents-not-printed";
  readonly affectedFiles: readonly string[];
  readonly text: string;
};

export type ResolveCommandOutput = CommandOutputBase<"resolve"> & {
  readonly projectOrPath: string;
  readonly action: ResolveAction;
  readonly conflicts: readonly ResolveConflict[];
  readonly availableAgents: readonly ResolveAgentOption[];
  readonly availableActions: readonly ResolveAvailableAction[];
  readonly prompt?: ResolvePrompt;
  readonly diff?: ResolveDiff;
  readonly requestedAgent?: ResolveAgent;
  readonly selectedConflictId?: string;
  readonly status: {
    readonly level: StatusLevel;
    readonly summary: string;
  };
  readonly nextActions: readonly RepairCommand[];
};
