import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import {
  BILLING_STORAGE_UNITS,
  FREE_AUTHORIZED_MACHINE_LIMIT,
  FREE_STORAGE_BYTES,
  PRO_STORAGE_BYTES,
  billingPlanLimits,
} from "@bowline/contracts";

describe("billing contract Convex mirror", () => {
  it("pins the tiny Convex mirror to the package constants", () => {
    const convexSource = readFileSync(
      join(process.cwd(), "convex/lib/billing.ts"),
      "utf8",
    );

    expect(convexSource).toContain(
      `export const BILLING_STORAGE_UNITS = "${BILLING_STORAGE_UNITS}"`,
    );
    expect(convexSource).toContain(
      `export const FREE_STORAGE_BYTES = ${FREE_STORAGE_BYTES.toLocaleString("en-US").replaceAll(",", "_")}`,
    );
    expect(convexSource).toContain(
      `export const PRO_STORAGE_BYTES = ${PRO_STORAGE_BYTES.toLocaleString("en-US").replaceAll(",", "_")}`,
    );
    expect(convexSource).toContain(
      `export const FREE_AUTHORIZED_MACHINE_LIMIT = ${FREE_AUTHORIZED_MACHINE_LIMIT}`,
    );
    expect(billingPlanLimits("free").storageBytesLimit).toBe(
      FREE_STORAGE_BYTES,
    );
    expect(billingPlanLimits("pro").storageBytesLimit).toBe(PRO_STORAGE_BYTES);
    expect(billingPlanLimits("pro").machineLimit).toBeNull();
  });
});
