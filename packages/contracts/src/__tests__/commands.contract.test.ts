import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  isAgentBudgetCommandOutput,
  isAgentContextCommandOutput,
  isAgentLeaseCreateCommandOutput,
  isAgentPromptCommandOutput,
  isAgentToolResult,
  isActionsCommandOutput,
  isBootstrapSshCommandOutput,
  isCommandErrorOutput,
  isContractCommandOutput,
  isDaemonCommandOutput,
  isDaemonServiceOutput,
  isDaemonStatusOutput,
  isDevicesCommandOutput,
  isDiagnosticsCollectCommandOutput,
  isDryRunCommandOutput,
  isEventsCommandOutput,
  isExplainCommandOutput,
  isHelpCommandOutput,
  isInitCommandOutput,
  isLoginCommandOutput,
  isPrewarmCommandOutput,
  isRecoveryCommandOutput,
  isResolveCommandOutput,
  isSearchCommandOutput,
  isStatusCommandOutput,
  isSymbolCommandOutput,
  isVersionCommandOutput,
  isWorkCleanupCommandOutput,
  isWorkDiffCommandOutput,
  isWorkLifecycleCommandOutput,
  isWorkListCommandOutput,
  isWorkonCommandOutput,
  statusNeedsAttention,
} from "../index";

const commandOutputGuards: Record<string, (value: unknown) => boolean> = {
  ActionsCommandOutput: isActionsCommandOutput,
  AgentBudgetCommandOutput: isAgentBudgetCommandOutput,
  AgentContextCommandOutput: isAgentContextCommandOutput,
  AgentLeaseCreateCommandOutput: isAgentLeaseCreateCommandOutput,
  AgentPromptCommandOutput: isAgentPromptCommandOutput,
  AgentToolResult: isAgentToolResult,
  BootstrapSshCommandOutput: isBootstrapSshCommandOutput,
  ContractCommandOutput: isContractCommandOutput,
  DaemonCommandOutput: isDaemonCommandOutput,
  DaemonServiceOutput: isDaemonServiceOutput,
  DaemonStatusOutput: isDaemonStatusOutput,
  DevicesCommandOutput: isDevicesCommandOutput,
  DiagnosticsCollectCommandOutput: isDiagnosticsCollectCommandOutput,
  DryRunCommandOutput: isDryRunCommandOutput,
  EventsCommandOutput: isEventsCommandOutput,
  ExplainCommandOutput: isExplainCommandOutput,
  HelpCommandOutput: isHelpCommandOutput,
  InitCommandOutput: isInitCommandOutput,
  LoginCommandOutput: isLoginCommandOutput,
  PrewarmCommandOutput: isPrewarmCommandOutput,
  RecoveryCommandOutput: isRecoveryCommandOutput,
  ResolveCommandOutput: isResolveCommandOutput,
  SearchCommandOutput: isSearchCommandOutput,
  StatusCommandOutput: isStatusCommandOutput,
  SymbolCommandOutput: isSymbolCommandOutput,
  VersionCommandOutput: isVersionCommandOutput,
  WorkCleanupCommandOutput: isWorkCleanupCommandOutput,
  WorkDiffCommandOutput: isWorkDiffCommandOutput,
  WorkLifecycleCommandOutput: isWorkLifecycleCommandOutput,
  WorkListCommandOutput: isWorkListCommandOutput,
  WorkonCommandOutput: isWorkonCommandOutput,
};

describe("workspace command contracts", () => {
  it("accepts the shared explain fixture", () => {
    const fixture = readCommandFixture("explain-env");

    expect(isExplainCommandOutput(fixture)).toBe(true);
    if (!isExplainCommandOutput(fixture)) return;

    expect(fixture.command).toBe("explain");
    expect(fixture.mode).toBe("project-env");
    expect(fixture.summary).not.toContain("API_KEY");
  });

  it("accepts the shared setup fixture", () => {
    const fixture = readCommandFixture("setup-blocked");

    expect(isPrewarmCommandOutput(fixture)).toBe(true);
    if (!isPrewarmCommandOutput(fixture)) return;

    expect(fixture.command).toBe("setup");
    expect(fixture.outcome.state).toBe("setup-blocked");
    expect(fixture.outcome.redactedSummary).not.toContain("SECRET_VALUE");
  });

  it("keeps observed-only status as attention-worthy", () => {
    expect(
      statusNeedsAttention({
        level: "attention",
        attentionItems: [
          "Workspace has been observed locally; sync has not started yet.",
        ],
      }),
    ).toBe(true);
  });

  it("accepts legacy command names in command errors", () => {
    expect(
      isCommandErrorOutput({
        command: "init",
        contractVersion: 3,
        generatedAt: "2026-06-27T12:00:00Z",
        status: "usage-error",
        error: {
          code: "ambiguous_root",
          message: "choose a root",
          recoverability: "user-action",
        },
      }),
    ).toBe(true);
  });

  it("accepts discovery and dry-run command fixtures", () => {
    expect(isHelpCommandOutput(readCommandFixture("help"))).toBe(true);
    expect(isVersionCommandOutput(readCommandFixture("version"))).toBe(true);
    expect(isContractCommandOutput(readCommandFixture("contract"))).toBe(true);
    expect(isDryRunCommandOutput(readCommandFixture("dry-run"))).toBe(true);
  });

  it("has guards for every advertised command output type", () => {
    const contract = readCommandFixture("contract");
    expect(isContractCommandOutput(contract)).toBe(true);
    if (!isContractCommandOutput(contract)) return;

    const missing = contract.commandOutputTypes.filter(
      (outputType) => commandOutputGuards[outputType] === undefined,
    );
    expect(missing).toEqual([]);
  });

  it("accepts daemon, diagnostics, and agent tool command surfaces", () => {
    expect(
      isDaemonCommandOutput({
        contractVersion: 3,
        command: "daemon start",
        generatedAt: "2026-06-29T12:00:00Z",
        daemon: { state: "starting", socket: "/tmp/bowline.sock", pid: 123 },
      }),
    ).toBe(true);

    expect(
      isDaemonStatusOutput({
        contractVersion: 3,
        command: "daemon status",
        generatedAt: "2026-06-29T12:00:00Z",
        daemon: {
          state: "running",
          socket: "/tmp/bowline.sock",
          protocol: "bowline.local",
          version: 1,
          daemonVersion: "0.0.0",
        },
        sync: { state: "ready" },
        service: {
          state: "running",
          unitPath: "/tmp/bowline.service",
        },
      }),
    ).toBe(true);

    expect(
      isDaemonServiceOutput({
        contractVersion: 3,
        command: "daemon install",
        generatedAt: "2026-06-29T12:00:00Z",
        service: {
          state: "installed",
          name: "bowline",
          unitPath: "/tmp/bowline.service",
        },
      }),
    ).toBe(true);

    expect(
      isDiagnosticsCollectCommandOutput({
        contractVersion: 3,
        command: "diagnostics collect",
        generatedAt: "2026-06-29T12:00:00Z",
        redactionRules: ["home-path"],
        bundle: "bowline diagnostics",
      }),
    ).toBe(true);

    expect(
      isAgentToolResult({
        requestId: "req_1",
        leaseId: "lease_1",
        tool: "complete_task",
        outcome: "allowed",
        summary: "completed",
        payload: { outputState: "completed" },
      }),
    ).toBe(true);
  });

  it("rejects malformed discovery, command, and cursor shapes", () => {
    const help = readCommandFixture("help") as Record<string, unknown>;
    const withoutContractVersion = { ...help };
    delete withoutContractVersion.contractVersion;
    expect(isHelpCommandOutput(withoutContractVersion)).toBe(false);

    expect(
      isDryRunCommandOutput({
        ...(readCommandFixture("dry-run") as Record<string, unknown>),
        command: "not a command",
      }),
    ).toBe(false);

    expect(
      isSearchCommandOutput({
        command: "search",
        contractVersion: 3,
        generatedAt: "2026-06-29T12:00:00Z",
        workspaceId: "ws_json",
        projectId: "proj_json",
        query: "needle",
        index: {
          state: "ready",
          source: "local",
          pathCount: 1,
          fileCount: 1,
          indexedBytes: 10,
          summary: "ready",
        },
        results: [],
        truncated: true,
        nextCursor: "offset:20",
        status: { level: "healthy", attentionItems: [] },
        nextActions: [],
      }),
    ).toBe(false);
  });

  it("rejects recovery output with one-time generated words", () => {
    const output = {
      action: "create",
      command: "recover",
      contractVersion: 3,
      generatedAt: "2026-06-24T12:00:00Z",
      generatedWords: "alpha beta gamma",
      recoveryKey: {
        createdAt: "2026-06-24T12:00:00Z",
        envelopeId: "rk_json",
        fingerprint: "rkp_json",
        lifecycle: "generated-unverified",
      },
      nextActions: [
        {
          command: "bowline connect linux-box --json",
          label: "Retry remote bootstrap",
        },
      ],
      workspaceId: "ws_json",
    };

    expect(isRecoveryCommandOutput(output)).toBe(false);
  });

  it("accepts blocked bootstrap sync output", () => {
    const output = {
      command: "connect",
      contractVersion: 3,
      generatedAt: "2026-06-24T12:00:00Z",
      host: "linux-box",
      nextActions: [],
      sync: "blocked",
      remoteStatus: {
        attentionItems: ["Remote bootstrap did not complete."],
        level: "limited",
      },
      root: "~/Code",
      secretStore: "server-local",
      steps: [
        {
          name: "install",
          state: "blocked",
          summary: "install failed",
        },
      ],
      trusted: false,
    };

    expect(isBootstrapSshCommandOutput(output)).toBe(true);
  });

  it("accepts resolve output without unavailable agent options", () => {
    const output = {
      action: "copy-prompt",
      availableActions: [
        {
          command: "bowline resolve /tmp/project --copy-prompt",
          label: "Print repair prompt",
        },
      ],
      availableAgents: [],
      command: "resolve",
      conflicts: [
        {
          activeView: "local",
          affectedFiles: ["apps/web/.env.local"],
          bundlePath: "/tmp/project/.bowline/conflicts/conflict_same_line",
          conflictKind: "text",
          containsSecrets: true,
          hasResolutionOverlay: true,
          id: "conflict_same_line",
          spans: [
            {
              baseEndLine: 4,
              baseStartLine: 4,
              localEndLine: 4,
              localStartLine: 4,
              path: "apps/web/.env.local",
              remoteEndLine: 4,
              remoteStartLine: 4,
            },
          ],
          state: "unresolved",
        },
      ],
      contractVersion: 3,
      generatedAt: "2026-06-24T12:00:00Z",
      nextActions: [
        {
          command: "bowline resolve /tmp/project --copy-prompt",
          label: "Print repair prompt",
        },
      ],
      projectOrPath: "/tmp/project",
      prompt: {
        bundlePath: "/tmp/project/.bowline/conflicts/conflict_same_line",
        conflictId: "conflict_same_line",
        redaction: "applied",
        resolutionPath:
          "/tmp/project/.bowline/conflicts/conflict_same_line/resolution",
        text: "Do not use Git. Write only to resolution/.",
      },
      status: {
        level: "attention",
        summary: "1 unresolved conflict bundle(s) found",
      },
    };

    expect(isResolveCommandOutput(output)).toBe(true);
    if (!isResolveCommandOutput(output)) return;

    expect(output.availableAgents).toEqual([]);
    expect(JSON.stringify(output.availableActions)).not.toContain("--agent");
    expect(output.prompt.text).not.toContain("SECRET_VALUE");
  });

  it("accepts resolve diff output", () => {
    const output = {
      action: "diff",
      availableActions: [
        {
          command: "bowline resolve /tmp/project --diff conflict_same_line",
          label: "Open diff conflict_same_line",
        },
      ],
      availableAgents: [],
      command: "resolve",
      conflicts: [
        {
          activeView: "local",
          affectedFiles: ["apps/web/.env.local"],
          bundlePath: "/tmp/project/.bowline/conflicts/conflict_same_line",
          containsSecrets: true,
          hasResolutionOverlay: true,
          id: "conflict_same_line",
          state: "unresolved",
        },
      ],
      contractVersion: 3,
      diff: {
        affectedFiles: ["apps/web/.env.local"],
        bundlePath: "/tmp/project/.bowline/conflicts/conflict_same_line",
        conflictId: "conflict_same_line",
        redaction: "contents-not-printed",
        text: "Conflict diff for `conflict_same_line`",
      },
      generatedAt: "2026-06-24T12:00:00Z",
      nextActions: [
        {
          command: "bowline resolve /tmp/project --diff conflict_same_line",
          label: "Open diff conflict_same_line",
        },
      ],
      projectOrPath: "/tmp/project",
      selectedConflictId: "conflict_same_line",
      status: {
        level: "attention",
        summary: "1 unresolved conflict bundle(s) found",
      },
    };

    expect(isResolveCommandOutput(output)).toBe(true);
  });

  it("accepts Phase 9 work view command fixtures", () => {
    expect(isWorkonCommandOutput(readCommandFixture("workon-created"))).toBe(
      true,
    );
    expect(isWorkDiffCommandOutput(readCommandFixture("work-review"))).toBe(
      true,
    );
    expect(
      isWorkLifecycleCommandOutput(readCommandFixture("work-accept")),
    ).toBe(true);
    expect(
      isWorkLifecycleCommandOutput(
        readCommandFixture("work-accept-review-ready"),
      ),
    ).toBe(true);
    expect(
      isWorkLifecycleCommandOutput(readCommandFixture("work-discard")),
    ).toBe(true);
  });

  it("accepts Phase 10 agent lease command fixtures without nonce or secrets", () => {
    const lease = readCommandFixture("agent-lease-create");
    const context = readCommandFixture("agent-context");
    const prompt = readCommandFixture("agent-prompt");

    expect(isAgentLeaseCreateCommandOutput(lease)).toBe(true);
    expect(isAgentContextCommandOutput(context)).toBe(true);
    expect(isAgentPromptCommandOutput(prompt)).toBe(true);
    expect(JSON.stringify([lease, context, prompt])).not.toContain("nonce");
    expect(JSON.stringify([lease, context, prompt])).not.toContain(
      "SECRET_VALUE",
    );
  });

  it("keeps review-ready work as attention, not limited", () => {
    const fixture = readStatusFixture("work-view-attention");

    expect(isStatusCommandOutput(fixture)).toBe(true);
    if (!isStatusCommandOutput(fixture)) return;

    expect(fixture.status.level).toBe("attention");
    expect(fixture.limits).toEqual([]);
    expect(JSON.stringify(fixture)).not.toContain("SECRET_VALUE");
  });
});

function readCommandFixture(name: string): unknown {
  const fixtureUrl = new URL(
    `../../../../tests/contracts/commands/${name}.json`,
    import.meta.url,
  );

  return JSON.parse(readFileSync(fixtureUrl, "utf8")) as unknown;
}

function readStatusFixture(name: string): unknown {
  const fixtureUrl = new URL(
    `../../../../tests/contracts/status/${name}.json`,
    import.meta.url,
  );

  return JSON.parse(readFileSync(fixtureUrl, "utf8")) as unknown;
}
