export type ConfigResult =
  | { readonly ok: true; readonly value: string }
  | { readonly ok: false; readonly error: "missing-env"; readonly key: string };

export function readRequiredEnv(
  env: Record<string, string | undefined>,
  key: string,
): ConfigResult {
  const value = env[key];
  return value ? { ok: true, value } : { error: "missing-env", key, ok: false };
}
