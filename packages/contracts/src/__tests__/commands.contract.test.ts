import { describe, expect, it } from "vitest";

import {
  COMMAND_NAMES,
  isBootstrapSshCommandOutput,
  isCommandErrorOutput,
  isContractCommandOutput,
  isContractSummaryCommandOutput,
  isDaemonCommandOutput,
  isDaemonServiceOutput,
  isDaemonStatusOutput,
  isDevicesCommandOutput,
  isDiagnosticsCollectCommandOutput,
  isDoctorCommandOutput,
  isDryRunCommandOutput,
  isEventsCommandOutput,
  isHelpCommandOutput,
  isHandoffCommandOutput,
  isHistoryCommandOutput,
  isLoginCommandOutput,
  isLogoutCommandOutput,
  isSetupProjectOutput,
  isScopedContractCommandOutput,
  isRecoveryCommandOutput,
  isResolveCommandOutput,
  isSetupCommandOutput,
  isStatusCommandOutput,
  isUpdateCommandOutput,
  isVersionCommandOutput,
  isWorkCleanupCommandOutput,
  isWorkDiffCommandOutput,
  isWorkLifecycleCommandOutput,
  isWorkListCommandOutput,
  isWorkCreateCommandOutput,
  EVENT_SCHEMA_VERSION,
  statusNeedsAttention,
} from "../index";
import {
  isRecord,
  manifestEntriesFor,
  readContractFixture,
} from "./support/contractFixtures";
import { isStringArray } from "../guard-primitives";

const commandOutputGuards: Record<string, (value: unknown) => boolean> = {
  BootstrapSshCommandOutput: isBootstrapSshCommandOutput,
  CommandNames: isStringArray,
  ContractCommandOutput: isContractCommandOutput,
  ContractSummaryCommandOutput: isContractSummaryCommandOutput,
  DaemonCommandOutput: isDaemonCommandOutput,
  DaemonServiceOutput: isDaemonServiceOutput,
  DaemonStatusOutput: isDaemonStatusOutput,
  DevicesCommandOutput: isDevicesCommandOutput,
  DiagnosticsCollectCommandOutput: isDiagnosticsCollectCommandOutput,
  DoctorCommandOutput: isDoctorCommandOutput,
  DryRunCommandOutput: isDryRunCommandOutput,
  EventsCommandOutput: isEventsCommandOutput,
  HelpCommandOutput: isHelpCommandOutput,
  HandoffCommandOutput: isHandoffCommandOutput,
  HistoryCommandOutput: isHistoryCommandOutput,
  LoginCommandOutput: isLoginCommandOutput,
  LogoutCommandOutput: isLogoutCommandOutput,
  SetupProjectOutput: isSetupProjectOutput,
  ScopedContractCommandOutput: isScopedContractCommandOutput,
  RecoveryCommandOutput: isRecoveryCommandOutput,
  ResolveCommandOutput: isResolveCommandOutput,
  SetupCommandOutput: isSetupCommandOutput,
  StatusCommandOutput: isStatusCommandOutput,
  UpdateCommandOutput: isUpdateCommandOutput,
  VersionCommandOutput: isVersionCommandOutput,
  WorkCleanupCommandOutput: isWorkCleanupCommandOutput,
  WorkDiffCommandOutput: isWorkDiffCommandOutput,
  WorkLifecycleCommandOutput: isWorkLifecycleCommandOutput,
  WorkListCommandOutput: isWorkListCommandOutput,
  WorkCreateCommandOutput: isWorkCreateCommandOutput,
};

describe("workspace command contracts", () => {
  it("keeps advertised command names in the shared fixture set", () => {
    const fixture = readContractFixture("command-names.json");
    expect(isStringArray(fixture)).toBe(true);
    if (!isStringArray(fixture)) return;

    // Ordering is presentation-only; serialized command-name membership is the contract.
    const fixtureNames = new Set(fixture);
    expect(fixtureNames.size).toBe(fixture.length);
    expect(new Set(COMMAND_NAMES)).toEqual(fixtureNames);
  });

  it("accepts every command fixture listed in the manifest", () => {
    for (const entry of manifestEntriesFor("commands")) {
      expect(entry.format).toBe("json");
      const guard = commandOutputGuards[entry.kind];
      expect(
        guard,
        `${entry.id} uses an unknown TypeScript decoder`,
      ).toBeDefined();
      if (guard === undefined) return;

      expect(guard(readContractFixture(entry.path))).toBe(true);
    }
  });

  it("accepts the shared setup fixture", () => {
    const fixture = readCommandFixture("setup-blocked");

    expect(isSetupProjectOutput(fixture)).toBe(true);
    if (!isSetupProjectOutput(fixture)) return;

    expect(fixture.command).toBe("setup");
    expect(fixture.outcome.state).toBe("setup-blocked");
    expect(fixture.outcome.redactedSummary).not.toContain("SECRET_VALUE");
  });

  it("accepts setup machine output", () => {
    const fixture = readCommandFixture("setup-machine");

    expect(isSetupCommandOutput(fixture)).toBe(true);
    if (!isSetupCommandOutput(fixture)) return;

    expect(fixture.command).toBe("setup");
    expect(fixture.root).toBe("~/Code");
    expect(fixture.login.status).toBe("account-authenticated");
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

  it("rejects removed command names in command errors", () => {
    expect(
      isCommandErrorOutput({
        command: "init",
        contractVersion: 8,
        generatedAt: "2026-06-27T12:00:00Z",
        status: "usage-error",
        error: {
          code: "ambiguous_root",
          message: "choose a root",
          recoverability: "user-action",
        },
      }),
    ).toBe(false);
  });

  it("accepts discovery and dry-run command fixtures", () => {
    expect(isHelpCommandOutput(readCommandFixture("help"))).toBe(true);
    expect(isVersionCommandOutput(readCommandFixture("version"))).toBe(true);
    expect(
      isUpdateCommandOutput({
        contractVersion: 8,
        command: "update",
        generatedAt: "2026-06-29T12:00:00Z",
        ok: true,
        currentVersion: "0.1.0",
        latestVersion: "0.1.1",
        updateAvailable: true,
        updateCommand:
          "curl -fsSL 'https://install.bowline.sh/install.sh' | sh",
      }),
    ).toBe(true);
    expect(isContractCommandOutput(readCommandFixture("contract"))).toBe(true);
    expect(
      isContractSummaryCommandOutput(readCommandFixture("contract-summary")),
    ).toBe(true);
    const scopedContract = readCommandFixture("contract-work-diff");
    expect(isScopedContractCommandOutput(scopedContract)).toBe(true);
    const scopedContractRecord = expectRecord(scopedContract);
    const descriptor = expectRecord(scopedContractRecord.descriptor);
    expect(descriptor.positionals).toEqual([
      { name: "target", required: false, repeatable: false },
    ]);
    expect(isDryRunCommandOutput(readCommandFixture("dry-run"))).toBe(true);
  });

  it("has guards for every advertised command output type", () => {
    const contract = readCommandFixture("contract");
    expect(isContractCommandOutput(contract)).toBe(true);
    if (!isContractCommandOutput(contract)) return;
    expect(contract.eventSchemaVersion).toBe(EVENT_SCHEMA_VERSION);

    const missing = contract.commandOutputTypes.filter(
      (outputType) => commandOutputGuards[outputType] === undefined,
    );
    expect(missing).toEqual([]);
  });

  it("accepts daemon and diagnostics command surfaces", () => {
    expect(
      isDaemonCommandOutput({
        contractVersion: 8,
        command: "daemon start",
        generatedAt: "2026-06-29T12:00:00Z",
        daemon: { state: "starting", socket: "/tmp/bowline.sock", pid: 123 },
      }),
    ).toBe(true);

    expect(
      isDaemonStatusOutput({
        contractVersion: 8,
        command: "daemon status",
        generatedAt: "2026-06-29T12:00:00Z",
        daemon: {
          state: "running",
          socket: "/tmp/bowline.sock",
          protocol: "bowline.local",
          version: 1,
          daemonVersion: "0.1.0",
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
        contractVersion: 8,
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
        contractVersion: 8,
        command: "diagnostics collect",
        generatedAt: "2026-06-29T12:00:00Z",
        redactionRules: ["home-path"],
        bundle: "bowline diagnostics",
      }),
    ).toBe(true);
  });

  it("rejects malformed discovery, command, and cursor shapes", () => {
    const help = expectRecord(readCommandFixture("help"));
    const withoutContractVersion = { ...help };
    delete withoutContractVersion.contractVersion;
    expect(isHelpCommandOutput(withoutContractVersion)).toBe(false);

    expect(
      isDryRunCommandOutput({
        ...expectRecord(readCommandFixture("dry-run")),
        command: "not a command",
      }),
    ).toBe(false);
  });

  it("rejects recovery output with one-time generated words", () => {
    const output = {
      action: "create",
      command: "recover",
      contractVersion: 8,
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
      contractVersion: 8,
      generatedAt: "2026-06-24T12:00:00Z",
      host: "linux-box",
      repairActions: [],
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
      contractVersion: 8,
      generatedAt: "2026-06-24T12:00:00Z",
      nextActions: [
        {
          command: "bowline resolve /tmp/project --copy-prompt",
          label: "Print repair prompt",
          mutates: false,
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

    const conflict = output.conflicts[0];
    const span = conflict?.spans[0];
    if (span === undefined) throw new Error("Expected a resolve conflict span");
    span.baseStartLine = -1.5;
    expect(isResolveCommandOutput(output)).toBe(true);
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
      contractVersion: 8,
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
          mutates: false,
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
    expect(
      isWorkCreateCommandOutput(readCommandFixture("work-create-created")),
    ).toBe(true);
    expect(
      isWorkCreateCommandOutput(readCommandFixture("work-create-reused")),
    ).toBe(true);
    expect(isWorkDiffCommandOutput(readCommandFixture("work-review"))).toBe(
      true,
    );
    expect(
      isWorkLifecycleCommandOutput(readCommandFixture("work-accept")),
    ).toBe(true);
    expect(
      isWorkLifecycleCommandOutput(readCommandFixture("work-accept-partial")),
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
  return readContractFixture(`commands/${name}.json`);
}

function readStatusFixture(name: string): unknown {
  return readContractFixture(`status/${name}.json`);
}

function expectRecord(value: unknown): Record<string, unknown> {
  expect(isRecord(value)).toBe(true);
  if (!isRecord(value)) {
    throw new Error("Expected fixture to be a JSON object");
  }

  return value;
}
