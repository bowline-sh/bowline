import { STATUS_FACT_AUTHORITIES } from "./generated/status-fact-authorities";
import type {
  StatusAttention,
  StatusAvailability,
  StatusFact,
  StatusSummary,
} from "./generated/wire-contracts";
import { isWireStatusFact } from "./generated/wire-guards";

export type StatusReducerScope = {
  readonly scope: StatusFact["scope"];
  readonly scopeId?: string;
  readonly aggregateChildren?: boolean;
};

export type StatusReducerOptions = StatusReducerScope & {
  readonly observedAt: string;
  readonly snapshotVersion: number;
};

const availabilityRank = { none: 0, degraded: 1, unavailable: 2 } as const;
const attentionRank = { none: 0, recommended: 1, required: 2 } as const;
export const MAX_STATUS_FACTS = 128;
const scopeRank = {
  account: 0,
  workspace: 1,
  project: 2,
  device: 2,
  session: 3,
  work_view: 3,
  lease: 4,
  path: 5,
} as const;

type Authority = {
  readonly authority: string;
  readonly validScopes: readonly string[];
  readonly availabilityImpact: StatusFact["availabilityImpact"];
  readonly attentionImpact: StatusFact["attentionImpact"];
  readonly impactsOverrideable: boolean;
  readonly actionKind?: string;
  readonly workspaceAffecting: boolean;
  readonly stalePolicy: "drop" | "retain" | "mark-stale";
  readonly priorityBand: number;
};

function authorityFor(kind: string): Authority | undefined {
  const registry: Readonly<Record<string, Authority>> = STATUS_FACT_AUTHORITIES;
  return registry[kind];
}

function appliesToScope(fact: StatusFact, target: StatusReducerScope): boolean {
  if (fact.scope === target.scope) {
    return target.scopeId === undefined || fact.scopeId === target.scopeId;
  }
  if (!target.aggregateChildren || target.scope !== "workspace") return false;
  const authority = authorityFor(fact.kind);
  return authority?.workspaceAffecting === true;
}

type ParsedTimestamp = {
  readonly epochSecond: number;
  readonly fractionalSecond: string;
};

const rfc3339Timestamp =
  /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:\d{2})$/u;

function parseTimestamp(value: string): ParsedTimestamp {
  const match = rfc3339Timestamp.exec(value);
  if (match === null) throw new Error(`invalid RFC 3339 timestamp: ${value}`);
  const [, dateAndTime, fractionalSecond = "", offset] = match;
  const epochMilliseconds = Date.parse(`${dateAndTime}${offset}`);
  if (!Number.isFinite(epochMilliseconds))
    throw new Error(`invalid RFC 3339 timestamp: ${value}`);
  return {
    epochSecond: epochMilliseconds / 1_000,
    fractionalSecond: fractionalSecond.replace(/0+$/u, ""),
  };
}

export function compareStatusTimestamps(left: string, right: string): number {
  const leftInstant = parseTimestamp(left);
  const rightInstant = parseTimestamp(right);
  if (leftInstant.epochSecond !== rightInstant.epochSecond)
    return leftInstant.epochSecond < rightInstant.epochSecond ? -1 : 1;
  const width = Math.max(
    leftInstant.fractionalSecond.length,
    rightInstant.fractionalSecond.length,
  );
  const leftFraction = leftInstant.fractionalSecond.padEnd(width, "0");
  const rightFraction = rightInstant.fractionalSecond.padEnd(width, "0");
  if (leftFraction === rightFraction) return 0;
  return leftFraction < rightFraction ? -1 : 1;
}

function compareNewest(left: StatusFact, right: StatusFact): StatusFact {
  const timestamp = compareStatusTimestamps(left.observedAt, right.observedAt);
  if (timestamp !== 0) return timestamp > 0 ? left : right;
  return left.id.localeCompare(right.id) >= 0 ? left : right;
}

function normalizedKnownFact(
  fact: StatusFact,
  now: string,
): StatusFact | undefined {
  const authority = authorityFor(fact.kind);
  if (authority === undefined) {
    const safeFact = { ...fact };
    delete safeFact.action;
    return {
      ...safeFact,
      availabilityImpact: "none",
      attentionImpact: "none",
    };
  }
  if (fact.source !== authority.authority)
    throw new Error(`${fact.kind}: source must be ${authority.authority}`);
  if (!authority.validScopes.includes(fact.scope))
    throw new Error(`${fact.kind}: invalid scope ${fact.scope}`);
  if (!authority.impactsOverrideable) {
    if (
      fact.availabilityImpact !== authority.availabilityImpact ||
      fact.attentionImpact !== authority.attentionImpact
    )
      throw new Error(
        `${fact.kind}: impacts are fixed by the authority registry`,
      );
  }
  const actionKind = authority.actionKind;
  if (fact.action !== undefined && fact.action.kind !== actionKind) {
    const safeFact = { ...fact };
    delete safeFact.action;
    return safeFact;
  }
  if (
    fact.staleAfter === undefined ||
    compareStatusTimestamps(fact.staleAfter, now) >= 0
  )
    return fact;
  if (authority.stalePolicy === "drop") return undefined;
  if (authority.stalePolicy === "retain") return fact;
  return {
    ...fact,
    availabilityImpact:
      availabilityRank[fact.availabilityImpact] < availabilityRank.degraded
        ? "degraded"
        : fact.availabilityImpact,
    attentionImpact:
      attentionRank[fact.attentionImpact] < attentionRank.recommended
        ? "recommended"
        : fact.attentionImpact,
  };
}

function compareFact(left: StatusFact, right: StatusFact): number {
  const attention =
    attentionRank[right.attentionImpact] - attentionRank[left.attentionImpact];
  if (attention !== 0) return attention;
  const availability =
    availabilityRank[right.availabilityImpact] -
    availabilityRank[left.availabilityImpact];
  if (availability !== 0) return availability;
  const leftBand = authorityFor(left.kind)?.priorityBand ?? 1000;
  const rightBand = authorityFor(right.kind)?.priorityBand ?? 1000;
  if (leftBand !== rightBand) return leftBand - rightBand;
  const specificity = scopeRank[right.scope] - scopeRank[left.scope];
  if (specificity !== 0) return specificity;
  return left.kind.localeCompare(right.kind) || left.id.localeCompare(right.id);
}

export function reduceStatusFacts(
  input: readonly StatusFact[],
  options: StatusReducerOptions,
): StatusSummary {
  parseTimestamp(options.observedAt);
  const deduped = new Map<string, StatusFact>();
  for (const candidate of input) {
    if (!isWireStatusFact(candidate))
      throw new Error("invalid StatusFact shape");
    if (!appliesToScope(candidate, options)) continue;
    const fact = normalizedKnownFact(candidate, options.observedAt);
    if (fact === undefined) continue;
    const key = `${fact.source}\u0000${fact.dedupeKey}`;
    const previous = deduped.get(key);
    deduped.set(
      key,
      previous === undefined ? fact : compareNewest(previous, fact),
    );
  }
  const facts = [...deduped.values()]
    .sort(compareFact)
    .slice(0, MAX_STATUS_FACTS);
  const availabilityImpact = facts.reduce<StatusFact["availabilityImpact"]>(
    (maximum, fact) =>
      availabilityRank[fact.availabilityImpact] > availabilityRank[maximum]
        ? fact.availabilityImpact
        : maximum,
    "none",
  );
  const attention = facts.reduce<StatusAttention>(
    (maximum, fact) =>
      attentionRank[fact.attentionImpact] > attentionRank[maximum]
        ? fact.attentionImpact
        : maximum,
    "none",
  );
  const availability: StatusAvailability =
    availabilityImpact === "none" ? "ready" : availabilityImpact;
  const primaryFactId = facts[0]?.id;
  const freshness = facts.some(
    (fact) =>
      fact.staleAfter !== undefined &&
      compareStatusTimestamps(fact.staleAfter, options.observedAt) < 0,
  )
    ? "stale"
    : "fresh";
  return {
    availability,
    attention,
    ...(primaryFactId === undefined ? {} : { primaryFactId }),
    facts,
    snapshotVersion: options.snapshotVersion,
    observedAt: options.observedAt,
    freshness,
  };
}

export function reduceStatusFactGroups(
  groups: readonly (readonly StatusFact[])[],
  options: StatusReducerOptions,
): StatusSummary {
  return reduceStatusFacts(groups.flat(), options);
}
