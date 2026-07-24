import type { RepairCommand, StatusLevel } from "./status";
import type { CommandOutputBase } from "./commands";

// The `--agent codex|claude|cursor` / `--copy-prompt` affordance only formats
// copyable conflict context (rehost map "bowline resolve" finding); this is
// the sole remaining consumer of the CLI-availability shape once the deleted
// agent-lease/MCP contract (packages/contracts/src/agent.ts) stopped owning it.
export type ResolveAgentCliName = "codex" | "claude" | "cursor";

export type ResolveAgentCliCapability = {
  readonly name: ResolveAgentCliName;
  readonly available: boolean;
  readonly command?: string;
  readonly supportsPromptFileLaunch: boolean;
  readonly supportsStdinLaunch: boolean;
  readonly supportsCwdSelection: boolean;
  readonly supportsNoninteractiveExecution: boolean;
  readonly supportsReceiptCapture: boolean;
  readonly degradedReason?: string;
};

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
  readonly capability: ResolveAgentCliCapability;
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
