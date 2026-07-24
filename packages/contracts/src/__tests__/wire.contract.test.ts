import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  CONTRACT_VERSION,
  EVENT_NAMES,
  WIRE_GUARDS,
  WIRE_SCHEMA_HASH,
  isWireDeviceApprovalAffordance,
  isWireEventName,
} from "../index";

// Each generated fixture file is parsed once and shared read-only across every
// obligation: the manifests point hundreds of obligations at the same
// boundary-values file, so re-reading per obligation reparsed the ~650KB file
// well over a thousand times and pushed the suite past the default timeout
// under load. Callers never mutate the returned value (expandBoundaryDescriptors
// builds fresh values), so caching parsed documents is safe.
const generatedJsonCache = new Map<string, unknown>();

function generatedJson(name: string): unknown {
  const cached = generatedJsonCache.get(name);
  if (cached !== undefined) return cached;
  const parsed: unknown = JSON.parse(
    readFileSync(
      new URL(`../../../../tests/contracts/generated/${name}`, import.meta.url),
      "utf8",
    ),
  );
  generatedJsonCache.set(name, parsed);
  return parsed;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function generatedRecord(name: string): Record<string, unknown> {
  const value = generatedJson(name);
  if (!isRecord(value)) throw new Error(`${name} must contain a JSON object`);
  return value;
}

function recordField(
  record: Readonly<Record<string, unknown>>,
  field: string,
): Record<string, unknown> {
  const value = record[field];
  if (!isRecord(value)) throw new Error(`${field} must be an object`);
  return value;
}

function arrayField(
  record: Readonly<Record<string, unknown>>,
  field: string,
): readonly unknown[] {
  const value = record[field];
  if (!Array.isArray(value)) throw new Error(`${field} must be an array`);
  return value;
}

function stringField(
  record: Readonly<Record<string, unknown>>,
  field: string,
): string {
  const value = record[field];
  if (typeof value !== "string") throw new Error(`${field} must be a string`);
  return value;
}

function numberField(
  record: Readonly<Record<string, unknown>>,
  field: string,
): number {
  const value = record[field];
  if (typeof value !== "number") throw new Error(`${field} must be a number`);
  return value;
}

// Length/count-boundary fixtures are committed as compact descriptors so the
// generated boundary-values file cannot balloon to megabytes for large maxItems
// / maxLength bounds; expand them to the exact string/array here, at load time,
// so the guards still see (and reject at) the precise just-outside value.
function expandBoundaryDescriptors(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(expandBoundaryDescriptors);
  if (!isRecord(value)) return value;
  const keys = Object.keys(value);
  if (keys.length === 1 && keys[0] === "$boundaryString") {
    const descriptor = recordField(value, "$boundaryString");
    const unit = stringField(descriptor, "unit");
    const count = numberField(descriptor, "count");
    return unit.repeat(count);
  }
  if (keys.length === 1 && keys[0] === "$boundaryArray") {
    const descriptor = recordField(value, "$boundaryArray");
    const item = descriptor["item"];
    const count = numberField(descriptor, "count");
    const expandedItem = expandBoundaryDescriptors(item);
    return Array.from({ length: count }, () => expandedItem);
  }
  if (keys.length === 1 && keys[0] === "$boundaryMap") {
    const descriptor = recordField(value, "$boundaryMap");
    const item = descriptor["value"];
    const count = numberField(descriptor, "count");
    const expandedItem = expandBoundaryDescriptors(item);
    return Object.fromEntries(
      Array.from({ length: count }, (_, index) => [
        `key${index}`,
        expandedItem,
      ]),
    );
  }
  if (keys.length === 1 && keys[0] === "$boundaryMapKey") {
    const descriptor = recordField(value, "$boundaryMapKey");
    const item = descriptor["value"];
    const unit = stringField(descriptor, "unit");
    const count = numberField(descriptor, "count");
    return { [unit.repeat(count)]: expandBoundaryDescriptors(item) };
  }
  return Object.fromEntries(
    Object.entries(value).map(([key, nested]) => [
      key,
      expandBoundaryDescriptors(nested),
    ]),
  );
}

function valueAtPath(path: string): unknown {
  const [fileName, pointer] = path.split("#");
  if (fileName === undefined) {
    throw new Error(`fixture obligation has no file name: ${path}`);
  }
  const document = generatedRecord(fileName.replace(/^\//, ""));
  const key = pointer?.replace(/^\//, "");
  const resolved =
    key === undefined || key.length === 0 ? document : document[key];
  return expandBoundaryDescriptors(resolved);
}

describe("generated wire contracts", () => {
  it("shares one schema hash, machine version, and event registry", () => {
    const registry = generatedRecord("registry.json");
    const enumValues = recordField(registry, "enumValues");

    expect(registry.schemaHash).toBe(WIRE_SCHEMA_HASH);
    expect(registry.machineContractVersion).toBe(CONTRACT_VERSION);
    expect(enumValues.EventName).toEqual(EVENT_NAMES);
    expect(arrayField(registry, "hostedEndpoints")).toHaveLength(38);
  });

  it("preserves unknown event names while tracking every known name", () => {
    expect(EVENT_NAMES).toHaveLength(62);
    expect(EVENT_NAMES.every(isWireEventName)).toBe(true);
    expect(isWireEventName("future.event_name")).toBe(true);
  });

  it("requires the canonical four-field local approval shape", () => {
    const approval = {
      requestId: "device-request:ws_fixture:dev_fixture",
      deviceName: "Fixture Mac",
      code: "<redacted>",
      approveCommand: "bowline device approve --code '<redacted>'",
    };

    expect(isWireDeviceApprovalAffordance(approval)).toBe(true);
    const withoutDeviceName = { ...approval } as Partial<typeof approval>;
    delete withoutDeviceName.deviceName;
    expect(isWireDeviceApprovalAffordance(withoutDeviceName)).toBe(false);
  });

  it("generates positive and negative obligations for every declaration", () => {
    const positive = generatedRecord("manifest.json");
    const negative = generatedRecord("negative-manifest.json");

    expect(positive.schemaHash).toBe(WIRE_SCHEMA_HASH);
    expect(negative.schemaHash).toBe(WIRE_SCHEMA_HASH);
    expect(arrayField(positive, "obligations").length).toBeGreaterThan(100);
    expect(arrayField(negative, "obligations").length).toBeGreaterThan(20);
  });

  it("executes every generated TypeScript fixture obligation", () => {
    const positive = arrayField(
      generatedRecord("manifest.json"),
      "obligations",
    );
    const negative = arrayField(
      generatedRecord("negative-manifest.json"),
      "obligations",
    );
    const negativeValues = generatedRecord("negative-values.json");

    for (const obligation of positive) {
      if (!isRecord(obligation))
        throw new Error("positive obligation must be an object");
      const declaration = stringField(obligation, "declaration");
      const valuePath = obligation.valuePath;
      const fixtureValue =
        typeof valuePath === "string"
          ? valueAtPath(valuePath)
          : obligation.value;
      expect(WIRE_GUARDS[declaration]?.(fixtureValue), declaration).toBe(true);
    }
    for (const obligation of negative) {
      if (!isRecord(obligation))
        throw new Error("negative obligation must be an object");
      const id = stringField(obligation, "id");
      const declaration = stringField(obligation, "declaration");
      const valuePath = obligation.valuePath;
      const fixtureValue =
        typeof valuePath === "string"
          ? valueAtPath(valuePath)
          : negativeValues[id];
      expect(WIRE_GUARDS[declaration]?.(fixtureValue), id).toBe(false);
    }
  });

  it("keeps sensitive approval material out of hosted contract projections", () => {
    const redaction = generatedRecord("redaction-manifest.json");
    const records = arrayField(redaction, "records").filter(isRecord);
    const approval = records.find(
      (record) => record.declaration === "DeviceApprovalAffordance",
    );

    expect(approval?.hostedAllowed).toBe(false);
    const sensitiveFields = approval
      ? arrayField(approval, "sensitiveFields")
      : [];
    expect(sensitiveFields.filter(isRecord).map((field) => field.name)).toEqual(
      ["code", "approveCommand"],
    );
  });
});
