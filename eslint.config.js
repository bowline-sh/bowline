import js from "@eslint/js";
import tseslint from "typescript-eslint";

// Plain-JavaScript tooling is not part of a TypeScript project, so type-aware
// rules cannot run against it. Keyed on file type, never a path list: adding a
// script must not require a config edit.
const scriptFiles = ["**/*.mjs", "**/*.cjs", "**/*.js"];

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
  // File length is not among them; scripts/check-file-lengths.mjs owns it.
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
        Buffer: "readonly",
        clearInterval: "readonly",
        console: "readonly",
        fetch: "readonly",
        Headers: "readonly",
        log: "readonly",
        parallel: "readonly",
        phase: "readonly",
        pipeline: "readonly",
        process: "readonly",
        Request: "readonly",
        setInterval: "readonly",
        setTimeout: "readonly",
        structuredClone: "readonly",
        URL: "readonly",
      },
    },
  },
);
