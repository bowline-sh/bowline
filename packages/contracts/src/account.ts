import type { AccountId, WorkOsOrganizationId, WorkOsUserId } from "./ids";

export const ACCOUNT_SESSION_ERROR_CODES = {
  expired: "account_session_expired",
  missing: "account_session_missing",
  revoked: "account_session_revoked",
} as const;

export type AccountSessionErrorCode =
  (typeof ACCOUNT_SESSION_ERROR_CODES)[keyof typeof ACCOUNT_SESSION_ERROR_CODES];

export type AccountLoginStatus =
  | "not-logged-in"
  | "login-pending"
  | "account-authenticated"
  | "expired";

export type AccountLoginState = {
  readonly status: AccountLoginStatus;
  readonly accountId?: AccountId;
  readonly workOsUserId?: WorkOsUserId;
  readonly workOsOrganizationId?: WorkOsOrganizationId;
  readonly userCode?: string;
  readonly verificationUri?: string;
  readonly verificationUriComplete?: string;
  readonly pollIntervalSeconds?: number;
  readonly expiresAt?: string;
  readonly authenticatedAt?: string;
};
