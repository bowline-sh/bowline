import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  STATUS_FACT_AUTHORITIES,
  MAX_STATUS_FACTS,
  reduceStatusFactGroups,
  reduceStatusFacts,
  isWireStatusFact,
  type StatusFact,
  type StatusReducerOptions,
} from "../wire";

type VectorFile = {
  readonly options: StatusReducerOptions;
  readonly cases: readonly {
    readonly id: string;
    readonly input: readonly StatusFact[];
    readonly expected: unknown;
  }[];
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isOptions(value: unknown): value is StatusReducerOptions {
  return (
    isRecord(value) &&
    value.scope === "workspace" &&
    typeof value.scopeId === "string" &&
    typeof value.aggregateChildren === "boolean" &&
    typeof value.observedAt === "string" &&
    typeof value.snapshotVersion === "number"
  );
}

function loadVectors(): VectorFile {
  const value: unknown = JSON.parse(
    readFileSync(
      new URL(
        "../../../../tests/contracts/generated/status-reducer-vectors.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  if (
    !isRecord(value) ||
    !isOptions(value.options) ||
    !Array.isArray(value.cases)
  )
    throw new Error("invalid reducer vector file");
  const cases = value.cases.map((item) => {
    if (
      !isRecord(item) ||
      typeof item.id !== "string" ||
      !Array.isArray(item.input) ||
      !item.input.every(isWireStatusFact)
    )
      throw new Error("invalid reducer vector case");
    return { id: item.id, input: item.input, expected: item.expected };
  });
  return { options: value.options, cases };
}

const vectors = loadVectors();

describe("canonical status reducer", () => {
  it("matches every generated conformance vector", () => {
    for (const vector of vectors.cases) {
      expect(
        reduceStatusFacts(vector.input, vectors.options),
        vector.id,
      ).toEqual(vector.expected);
    }
  });

  it("is invariant under permutations and input grouping", () => {
    const contradictory = vectors.cases.find(
      (vector) => vector.id === "contradictory.offline-and-conflict",
    );
    if (contradictory === undefined)
      throw new Error("missing contradictory vector");
    const expected = reduceStatusFacts(contradictory.input, vectors.options);
    expect(
      reduceStatusFacts([...contradictory.input].reverse(), vectors.options),
    ).toEqual(expected);
    expect(
      reduceStatusFactGroups(
        contradictory.input.map((fact) => [fact]),
        vectors.options,
      ),
    ).toEqual(expected);
  });

  it("publishes one sorted authority registry with hosting metadata", () => {
    const authorities = Object.values(STATUS_FACT_AUTHORITIES);
    expect(authorities.length).toBeGreaterThan(10);
    expect(
      authorities.every((entry) => typeof entry.hostedAllowed === "boolean"),
    ).toBe(true);
    expect(
      authorities.every(
        (entry) => typeof entry.workspaceAffecting === "boolean",
      ),
    ).toBe(true);
  });

  it("rejects contradictory source and fixed-impact claims", () => {
    const base = vectors.cases.find((vector) =>
      vector.id.includes("cross-product"),
    )?.input[0];
    if (base === undefined) throw new Error("missing base fact");
    expect(() =>
      reduceStatusFacts(
        [{ ...base, kind: "network.offline", source: "status-reducer" }],
        vectors.options,
      ),
    ).toThrow(/source must be/u);
    expect(() =>
      reduceStatusFacts(
        [
          {
            ...base,
            kind: "sync.conflict_unresolved",
            source: "local-conflict-store",
            availabilityImpact: "unavailable",
          },
        ],
        vectors.options,
      ),
    ).toThrow(/impacts are fixed/u);
  });

  it("bounds facts deterministically without changing the strongest axes", () => {
    const base = vectors.cases.find((vector) =>
      vector.id.includes("cross-product"),
    )?.input[0];
    if (base === undefined) throw new Error("missing base fact");
    const input = Array.from({ length: MAX_STATUS_FACTS + 20 }, (_, index) => ({
      ...base,
      id: `bounded-${index.toString().padStart(3, "0")}`,
      dedupeKey: `bounded-${index.toString().padStart(3, "0")}`,
    }));

    const forward = reduceStatusFacts(input, vectors.options);
    const reverse = reduceStatusFacts([...input].reverse(), vectors.options);

    expect(forward.facts).toHaveLength(MAX_STATUS_FACTS);
    expect(reverse).toEqual(forward);
  });
});
