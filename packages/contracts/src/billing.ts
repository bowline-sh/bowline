export const BILLING_STORAGE_UNITS = "decimal-gb";

export const FREE_STORAGE_BYTES = 10_000_000_000;
export const PRO_STORAGE_BYTES = 250_000_000_000;
export const FREE_AUTHORIZED_MACHINE_LIMIT = 3;

export const billingPlanLimits = {
  free: {
    machineLimit: FREE_AUTHORIZED_MACHINE_LIMIT,
    storageBytesLimit: FREE_STORAGE_BYTES,
    tier: "free",
  },
  pro: {
    machineLimit: null,
    storageBytesLimit: PRO_STORAGE_BYTES,
    tier: "pro",
  },
  team: {
    machineLimit: null,
    storageBytesLimit: null,
    tier: "team",
  },
} as const;

export type BillingPlanTier = keyof typeof billingPlanLimits;
export type BillingPlanLimits = (typeof billingPlanLimits)[BillingPlanTier];

export function billingPlanLimitsFor(tier: BillingPlanTier): BillingPlanLimits {
  return billingPlanLimits[tier];
}

export function totalStoredBytes(input: {
  readonly storageBytesCurrent: number;
  readonly storageBytesRetained: number;
}): number {
  return input.storageBytesCurrent + input.storageBytesRetained;
}
