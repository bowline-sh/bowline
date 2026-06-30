export const BILLING_PLAN_TIERS = ["free", "pro", "team"] as const;
export type BillingPlanTier = (typeof BILLING_PLAN_TIERS)[number];

export const BILLING_STORAGE_UNITS = "decimal-gb";

export const FREE_STORAGE_BYTES = 5_000_000_000;
export const PRO_STORAGE_BYTES = 250_000_000_000;
export const FREE_AUTHORIZED_MACHINE_LIMIT = 2;

export type BillingPlanLimits = {
  readonly machineLimit: number | null;
  readonly storageBytesLimit: number | null;
  readonly tier: BillingPlanTier;
};

export const BILLING_PLAN_LIMITS = {
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
} as const satisfies Record<BillingPlanTier, BillingPlanLimits>;

export function billingPlanLimits(tier: BillingPlanTier): BillingPlanLimits {
  return BILLING_PLAN_LIMITS[tier];
}

export function totalStoredBytes(input: {
  readonly storageBytesCurrent: number;
  readonly storageBytesRetained: number;
}): number {
  return input.storageBytesCurrent + input.storageBytesRetained;
}
