import js from "@eslint/js";
import tseslint from "typescript-eslint";

export default tseslint.config(
  {
    ignores: [
      "**/dist/**",
      "**/.worktrees/**",
      "**/node_modules/**",
      "**/convex/_generated/**",
      "**/target/**",
      "**/routeTree.gen.ts",
      "**/.source/**",
      "tests/fixtures/**",
      "**/fixtures/**",
      "docs/**",
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
    files: ["packages/contracts/src/guards.ts"],
    rules: {
      complexity: ["error", { max: 35 }],
      "max-lines": [
        "error",
        { max: 1900, skipBlankLines: true, skipComments: true },
      ],
    },
  },
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
        { max: 220, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: ["packages/control-plane/src/cloud/internal/store.ts"],
    rules: {
      "max-lines-per-function": [
        "error",
        { max: 360, skipBlankLines: true, skipComments: true },
      ],
    },
  },
  {
    files: ["**/scripts/**/*.mjs", "eslint.config.js"],
    extends: [tseslint.configs.disableTypeChecked],
    languageOptions: {
      globals: {
        console: "readonly",
        process: "readonly",
      },
    },
  },
);
