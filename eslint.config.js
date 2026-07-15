import js from "@eslint/js";
import tseslint from "typescript-eslint";

const scriptFiles = [
  "apps/docs/scripts/check-agent-readiness.mjs",
  "apps/docs/scripts/check-docs.mjs",
  "apps/web/scripts/generate-agent-auth-jwk.mjs",
  "eslint.config.js",
  "packages/contracts/scripts/generate-guards.mjs",
  "plans/oracle-scan/.author-workflow.js",
  "plans/oracle-scan/.review-workflow.js",
  "plans/oracle-scan/.verify-workflow.js",
  "scripts/check-architecture-fixtures.mjs",
  "scripts/check-architecture-imports.mjs",
  "scripts/check-cli-docs.mjs",
  "scripts/check-contracts-manifest.mjs",
  "scripts/check-current-state-authorities.mjs",
  "scripts/check-current-state-authorities.test.mjs",
  "scripts/check-current-state-registry.mjs",
  "scripts/current-state-authority-core.mjs",
  "scripts/current-state-acceptance-proof.mjs",
  "scripts/current-state-acceptance-proof.test.mjs",
  "scripts/check-contracts-codegen.mjs",
  "scripts/check-examples.mjs",
  "scripts/check-file-lengths.mjs",
  "scripts/check-generated-artifacts.mjs",
  "scripts/check-hosted-config.mjs",
  "scripts/check-install-script.mjs",
  "scripts/check-hosted-endpoint-inventory.mjs",
  "scripts/check-no-opaque-transport.mjs",
  "scripts/check-package-scripts.mjs",
  "scripts/check-public-export.mjs",
  "scripts/check-runtime-toolchains.mjs",
  "scripts/check-rust-boundaries.mjs",
  "scripts/check-toolchain-declarations.mjs",
  "scripts/check-whitespace.mjs",
  "scripts/check-work-view-authorities.mjs",
  "scripts/deploy-public.mjs",
  "scripts/deploy.mjs",
  "scripts/export-public.mjs",
  "scripts/hosted-daemon-loop-smoke.mjs",
  "scripts/plans.mjs",
  "scripts/plans.test.mjs",
  "scripts/prod-smoke.mjs",
  "scripts/release-assets.mjs",
  "scripts/release-authenticity-smoke.mjs",
  "scripts/release-signing.mjs",
  "scripts/release-version.mjs",
  "scripts/release.mjs",
  "scripts/verify.mjs",
  "scripts/verify.test.mjs",
  "scripts/wire-contracts/*.mjs",
  "scripts/sync-hosted-smoke.mjs",
  "scripts/sync-remote-smoke.mjs",
  "scripts/sync-two-device-smoke.mjs",
  "scripts/watcher-wake-smoke.mjs",
  "tests/cli-flows/cli-contract.test.mjs",
];

export default tseslint.config(
  {
    ignores: [
      "**/dist/**",
      "**/.agents/**",
      "**/.claude/**",
      "**/.worktrees/**",
      "**/node_modules/**",
      "**/convex/_generated/**",
      "**/target/**",
      "**/routeTree.gen.ts",
      "**/.source/**",
      "tests/fixtures/**",
      "**/fixtures/**",
      "docs/**",
      "plans/.simplification-wave-workflow.js",
      "plans/archive/oracle-scan-raw/**",
      "reports/**",
      "transcripts/**",
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  {
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      "@typescript-eslint/consistent-type-imports": "error",
      "@typescript-eslint/no-confusing-void-expression": "off",
      "@typescript-eslint/no-extraneous-class": "error",
      "@typescript-eslint/no-floating-promises": "error",
      "@typescript-eslint/no-misused-promises": "error",
      "@typescript-eslint/no-unnecessary-condition": "error",
      "@typescript-eslint/no-unsafe-type-assertion": "error",
      "@typescript-eslint/restrict-template-expressions": [
        "error",
        { allowBoolean: true, allowNumber: true },
      ],
      "max-lines": [
        "error",
        { max: 2000, skipBlankLines: true, skipComments: true },
      ],
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            {
              group: ["@bowline/*/internal", "@bowline/*/internal/**"],
              message:
                "Import from the module public entrypoint instead of internal files.",
            },
          ],
        },
      ],
    },
  },
  {
    files: [
      "apps/*/src/**/*.{ts,tsx}",
      "packages/*/src/**/*.{ts,tsx}",
      "packages/*/convex/**/*.ts",
    ],
    ignores: [
      "**/*.test.{ts,tsx}",
      "**/__tests__/**",
      "**/test/**",
      "**/routeTree.gen.ts",
    ],
    rules: {
      complexity: ["error", { max: 24 }],
      "max-lines": [
        "error",
        { max: 800, skipBlankLines: true, skipComments: true },
      ],
      "max-lines-per-function": [
        "error",
        {
          max: 180,
          skipBlankLines: true,
          skipComments: true,
        },
      ],
    },
  },
  {
    files: ["packages/control-plane/convex/**/*.ts"],
    ignores: ["**/*.test.ts", "**/__tests__/**"],
    rules: {
      "no-restricted-syntax": [
        "error",
        {
          selector: 'ThrowStatement > NewExpression[callee.name="Error"]',
          message:
            "Throw ConvexError({ code, message }) — prod redacts plain Error messages and clients see an undifferentiated server error.",
        },
      ],
    },
  },
  // Per-file overrides below are ratchets: they may only shrink or be deleted.
  // Never add a new one or raise an existing cap (see AGENTS.md quality rules).
  {
    files: ["packages/control-plane/convex/devices.ts"],
    rules: {
      "max-lines": [
        "error",
        { max: 1150, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: ["packages/control-plane/convex/billing.ts"],
    rules: {
      "max-lines": [
        "error",
        { max: 1000, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: ["packages/control-plane/convex/usage_rollups.ts"],
    rules: {
      complexity: ["error", { max: 35 }],
    },
  },
  {
    files: ["apps/web/src/components/marketing/hero/hero-stage-crt.tsx"],
    rules: {
      "max-lines-per-function": [
        "error",
        { max: 240, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: ["apps/web/src/routes/alternatives/$competitor.tsx"],
    rules: {
      "max-lines-per-function": [
        "error",
        { max: 210, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: scriptFiles,
    extends: [tseslint.configs.disableTypeChecked],
    languageOptions: {
      globals: {
        agent: "readonly",
        console: "readonly",
        fetch: "readonly",
        log: "readonly",
        parallel: "readonly",
        phase: "readonly",
        pipeline: "readonly",
        process: "readonly",
        Request: "readonly",
      },
    },
  },
);
