function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

type NumericRule = "nonnegative" | "nonnegative-integer";
type NumericPath = {
  readonly path: readonly string[];
  readonly rule: NumericRule;
};

const NONNEGATIVE = "nonnegative" satisfies NumericRule;
const NONNEGATIVE_INTEGER = "nonnegative-integer" satisfies NumericRule;

const numericRefinements: Readonly<Record<string, readonly NumericPath[]>> = {
  AgentPromptCommandOutput: [
    { path: ["prompt", "recipeVersion"], rule: NONNEGATIVE },
  ],
  BootstrapSshCommandOutput: [
    { path: ["nextRequiredPhase"], rule: NONNEGATIVE },
  ],
  CommandErrorOutput: [
    { path: ["error", "retryAfterSeconds"], rule: NONNEGATIVE },
  ],
  ContractCommandOutput: [
    { path: ["protocolVersion"], rule: NONNEGATIVE_INTEGER },
    { path: ["eventSchemaVersion"], rule: NONNEGATIVE_INTEGER },
    {
      path: ["commands", "*", "boundedOutput", "defaultLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
    {
      path: ["commands", "*", "boundedOutput", "maxLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
  ],
  ContractSummaryCommandOutput: [
    { path: ["protocolVersion"], rule: NONNEGATIVE_INTEGER },
    { path: ["eventSchemaVersion"], rule: NONNEGATIVE_INTEGER },
  ],
  DaemonCommandOutput: [
    { path: ["daemon", "version"], rule: NONNEGATIVE_INTEGER },
    { path: ["daemon", "pid"], rule: NONNEGATIVE_INTEGER },
  ],
  DaemonStatusOutput: [
    { path: ["daemon", "version"], rule: NONNEGATIVE_INTEGER },
    { path: ["daemon", "pid"], rule: NONNEGATIVE_INTEGER },
  ],
  EventsCommandOutput: [
    { path: ["eventWatermarks", "eventLagMs"], rule: NONNEGATIVE },
  ],
  HandoffCommandOutput: [
    {
      path: ["candidates", "*", "modifiedAtUnixSeconds"],
      rule: NONNEGATIVE_INTEGER,
    },
  ],
  HelpCommandOutput: [
    {
      path: ["commands", "*", "boundedOutput", "defaultLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
    {
      path: ["commands", "*", "boundedOutput", "maxLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
  ],
  HistoryCommandOutput: [
    ...historySummaryPaths(["restorePoints", "*", "summary"]),
    ...historySummaryPaths(["diffSummary"]),
  ],
  LoginCommandOutput: [
    { path: ["account", "pollIntervalSeconds"], rule: NONNEGATIVE },
  ],
  RecoveryCommandOutput: [
    { path: ["encryptedGrant", "keyEpoch"], rule: NONNEGATIVE },
  ],
  ScopedContractCommandOutput: [
    { path: ["protocolVersion"], rule: NONNEGATIVE_INTEGER },
    { path: ["eventSchemaVersion"], rule: NONNEGATIVE_INTEGER },
    {
      path: ["descriptor", "boundedOutput", "defaultLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
    {
      path: ["descriptor", "boundedOutput", "maxLimit"],
      rule: NONNEGATIVE_INTEGER,
    },
  ],
  SetupCommandOutput: [
    { path: ["login", "pollIntervalSeconds"], rule: NONNEGATIVE },
  ],
  StatusCommandOutput: [
    ...syncQueuePaths(),
    { path: ["eventWatermarks", "eventLagMs"], rule: NONNEGATIVE },
    { path: ["workspaceSummary", "totalProjects"], rule: NONNEGATIVE },
    ...observedWorkspacePaths(),
  ],
  VersionCommandOutput: [
    { path: ["protocolVersion"], rule: NONNEGATIVE_INTEGER },
  ],
  WatchFrame: [{ path: ["sequence"], rule: NONNEGATIVE_INTEGER }],
};

function historySummaryPaths(
  prefix: readonly string[],
): readonly NumericPath[] {
  return [
    "filesChanged",
    "filesAdded",
    "filesModified",
    "filesDeleted",
    "filesRenamed",
    "binaryOrLargeFilesChanged",
    "envKeysChanged",
  ].map((field) => ({ path: [...prefix, field], rule: NONNEGATIVE }));
}

function syncQueuePaths(): readonly NumericPath[] {
  return [
    "queued",
    "claimed",
    "waitingRetry",
    "blockedOffline",
    "reconciliationRequired",
    "attention",
    "completed",
  ].map((field) => ({
    path: ["syncQueue", field],
    rule: NONNEGATIVE_INTEGER,
  }));
}

function observedWorkspacePaths(): readonly NumericPath[] {
  return [
    "repoCount",
    "noRemoteRepoCount",
    "staleRemoteTrackingRepoCount",
    "gitPartialProjectCount",
    "gitUnavailableProjectCount",
    "generatedPathCount",
    "dependencyPathCount",
    "envFileCount",
    "untrackedFileCount",
    "localOnlyPathCount",
    "blockedPathCount",
    "workspaceSyncPathCount",
  ].map((field) => ({
    path: ["workspaceSummary", "observed", field],
    rule: NONNEGATIVE,
  }));
}

function matchesNumericRule(value: unknown, rule: NumericRule): boolean {
  return (
    value === undefined ||
    (typeof value === "number" &&
      Number.isFinite(value) &&
      value >= 0 &&
      (rule !== NONNEGATIVE_INTEGER || Number.isInteger(value)))
  );
}

function matchesNumericPath(
  value: unknown,
  path: readonly string[],
  rule: NumericRule,
): boolean {
  if (value === undefined) return true;
  const [segment, ...remaining] = path;
  if (segment === undefined) return matchesNumericRule(value, rule);
  if (segment === "*") {
    return (
      Array.isArray(value) &&
      value.every((item) => matchesNumericPath(item, remaining, rule))
    );
  }
  return isRecord(value) && matchesNumericPath(value[segment], remaining, rule);
}

function hasValidNumericRefinements(name: string, value: unknown): boolean {
  const refinements = numericRefinements[name] ?? [];
  return refinements.every(({ path, rule }) =>
    matchesNumericPath(value, path, rule),
  );
}

function hasValidAgentWriteTargets(value: unknown): boolean {
  if (Array.isArray(value)) return value.every(hasValidAgentWriteTargets);
  if (!isRecord(value)) return true;
  if (
    value.writeTargetMode === "work-view" &&
    (typeof value.workViewId !== "string" ||
      typeof value.workViewPath !== "string")
  ) {
    return false;
  }
  return Object.values(value).every(hasValidAgentWriteTargets);
}

function isHandoffOutcomeValid(value: unknown): boolean {
  if (!isRecord(value)) return false;
  switch (value.outcome) {
    case "dry_run":
      return (
        value.plan !== undefined &&
        value.receipt === undefined &&
        value.error === undefined
      );
    case "confirmation_required":
      return (
        value.error !== undefined &&
        value.receipt === undefined &&
        value.plan === undefined
      );
    case "receipt": {
      if (!isRecord(value.receipt)) return false;
      return (
        value.plan !== undefined &&
        value.error === undefined &&
        value.receipt.monitoring === false &&
        value.receipt.workspaceLock === false &&
        value.receipt.agentRuntimeVerified === false &&
        (value.receipt.sessionMode !== "resume_existing" ||
          value.receipt.sameSessionConcurrencyRisk === true)
      );
    }
    case "error":
      return value.error !== undefined && value.receipt === undefined;
    default:
      return false;
  }
}

function hasNonEmptyApprovalFields(value: unknown): boolean {
  return (
    isRecord(value) &&
    typeof value.requestId === "string" &&
    value.requestId.length > 0 &&
    typeof value.deviceName === "string" &&
    value.deviceName.length > 0 &&
    typeof value.approveCommand === "string" &&
    value.approveCommand.length > 0
  );
}

function hasValidWorkLifecycle(value: unknown): boolean {
  if (!isRecord(value)) return false;
  switch (value.command) {
    case "accept":
    case "work accept":
      return value.action === "accepted" || value.action === "review-ready";
    case "discard":
    case "work discard":
      return value.action === "discarded";
    case "restore":
    case "work restore":
      return value.action === "restored";
    default:
      return false;
  }
}

function hasValidHistoryCursor(value: unknown): boolean {
  return (
    isRecord(value) &&
    (value.nextCursor === undefined ||
      (typeof value.nextCursor === "string" &&
        /^v1:\d+$/u.test(value.nextCursor)))
  );
}

export function guardRefinement(name: string, value: unknown): boolean {
  if (!hasValidNumericRefinements(name, value)) return false;
  switch (name) {
    case "AgentContextCommandOutput":
    case "AgentLeaseCreateCommandOutput":
    case "AgentPromptCommandOutput":
      return hasValidAgentWriteTargets(value);
    case "DeviceApprovalAffordance":
      return hasNonEmptyApprovalFields(value);
    case "DeviceApprovalAffordances":
      return Array.isArray(value) && value.every(hasNonEmptyApprovalFields);
    case "HandoffCommandOutput":
      return isHandoffOutcomeValid(value);
    case "HistoryCommandOutput":
      return hasValidHistoryCursor(value);
    case "RecoveryCommandOutput":
      return isRecord(value) && !("generatedWords" in value);
    case "StatusCommandOutput":
      return isRecord(value) && typeof value.workspaceId === "string";
    case "WorkLifecycleCommandOutput":
      return hasValidWorkLifecycle(value);
    default:
      return true;
  }
}
