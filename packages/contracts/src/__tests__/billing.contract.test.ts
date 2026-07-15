import { describe, expect, it } from "vitest";

import {
  BILLING_STORAGE_UNITS,
  billingPlanLimits,
  totalStoredBytes,
} from "../billing";

describe("billing plan contract", () => {
  it("pins Free to three machines and 10 decimal GB", () => {
    expect(BILLING_STORAGE_UNITS).toBe("decimal-gb");
    expect(billingPlanLimits.free).toEqual({
      machineLimit: 3,
      storageBytesLimit: 10_000_000_000,
      tier: "free",
    });
  });

  it("pins Pro storage without inventing a machine cap", () => {
    expect(billingPlanLimits.pro).toEqual({
      machineLimit: null,
      storageBytesLimit: 250_000_000_000,
      tier: "pro",
    });
  });

  it("keeps Team reserved", () => {
    expect(billingPlanLimits.team).toEqual({
      machineLimit: null,
      storageBytesLimit: null,
      tier: "team",
    });
  });

  it("counts retained history in total stored bytes", () => {
    expect(
      totalStoredBytes({
        storageBytesCurrent: 12,
        storageBytesRetained: 30,
      }),
    ).toBe(42);
  });
});
