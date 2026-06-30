import { describe, expect, it } from "vitest";

import { readRequiredEnv } from "../index";

describe("config contract", () => {
  it("returns an env value when present", () => {
    expect(readRequiredEnv({ BOWLINE_ENV: "dev" }, "BOWLINE_ENV")).toEqual({
      ok: true,
      value: "dev",
    });
  });

  it("returns a typed error when missing", () => {
    expect(readRequiredEnv({}, "BOWLINE_ENV")).toEqual({
      error: "missing-env",
      key: "BOWLINE_ENV",
      ok: false,
    });
  });
});
