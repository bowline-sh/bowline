#!/usr/bin/env node

import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { format, resolveConfig } from "prettier";
import ts from "typescript";

const packageRoot = path.resolve(import.meta.dirname, "..");
const targetsPath = path.join(packageRoot, "src/guard-targets.ts");
const outputPath = path.join(packageRoot, "src/generated-guards.ts");
const checkOnly = process.argv.includes("--check");

function loadProgram() {
  const configPath = ts.findConfigFile(
    packageRoot,
    ts.sys.fileExists,
    "tsconfig.json",
  );
  if (configPath === undefined) {
    throw new Error("packages/contracts/tsconfig.json not found");
  }
  const config = ts.readConfigFile(configPath, ts.sys.readFile);
  if (config.error !== undefined) {
    throw new Error(
      ts.flattenDiagnosticMessageText(config.error.messageText, "\n"),
    );
  }
  const parsed = ts.parseJsonConfigFileContent(
    config.config,
    ts.sys,
    packageRoot,
  );
  return ts.createProgram(parsed.fileNames, parsed.options);
}

function hasFlag(type, flag) {
  return (type.flags & flag) !== 0;
}

function createSchemaBuilder(checker, fallbackNode) {
  const definitions = [];
  const definitionIds = new Map();

  function primitiveSchema(type) {
    if (hasFlag(type, ts.TypeFlags.Any | ts.TypeFlags.Unknown)) {
      return { kind: "any" };
    }
    if (hasFlag(type, ts.TypeFlags.Never)) return { kind: "never" };
    if (hasFlag(type, ts.TypeFlags.Undefined | ts.TypeFlags.Void)) {
      return { kind: "undefined" };
    }
    if (hasFlag(type, ts.TypeFlags.Null)) {
      return { kind: "literal", value: null };
    }
    if (hasFlag(type, ts.TypeFlags.StringLike)) {
      return type.isStringLiteral()
        ? { kind: "literal", value: type.value }
        : { kind: "string" };
    }
    if (hasFlag(type, ts.TypeFlags.NumberLike)) {
      return type.isNumberLiteral()
        ? { kind: "literal", value: type.value }
        : { kind: "number" };
    }
    if (hasFlag(type, ts.TypeFlags.BooleanLike)) {
      if (type.intrinsicName === "true") {
        return { kind: "literal", value: true };
      }
      if (type.intrinsicName === "false") {
        return { kind: "literal", value: false };
      }
      return { kind: "boolean" };
    }
    return null;
  }

  function isStringBrand(type) {
    return (
      type.isIntersection() &&
      type.types.some((part) => hasFlag(part, ts.TypeFlags.StringLike)) &&
      type.types.some(
        (part) => checker.getPropertyOfType(part, "__brand") !== undefined,
      )
    );
  }

  function refinementName(type) {
    const symbol = type.aliasSymbol ?? type.getSymbol();
    if (symbol === undefined || symbol.name.startsWith("__")) return null;
    return symbol.name;
  }

  function refinedSchema(type, schema, applyRefinement) {
    const refinement = applyRefinement ? refinementName(type) : null;
    return refinement === null ? schema : { ...schema, refinement };
  }

  function schemaFor(type, applyRefinement = true) {
    const primitive = primitiveSchema(type);
    if (primitive !== null) {
      return refinedSchema(type, primitive, applyRefinement);
    }
    if (isStringBrand(type)) {
      return refinedSchema(type, { kind: "string" }, applyRefinement);
    }
    if (type.isUnion()) {
      return refinedSchema(
        type,
        { kind: "union", variants: type.types.map(schemaFor) },
        applyRefinement,
      );
    }
    if (checker.isArrayType(type) || checker.isTupleType(type)) {
      const typeArguments = checker.getTypeArguments(type);
      if (checker.isTupleType(type)) {
        return refinedSchema(
          type,
          { kind: "tuple", items: typeArguments.map(schemaFor) },
          applyRefinement,
        );
      }
      return refinedSchema(
        type,
        {
          kind: "array",
          item: schemaFor(typeArguments[0] ?? checker.getAnyType()),
        },
        applyRefinement,
      );
    }

    const existing = definitionIds.get(type.id);
    if (existing !== undefined) {
      return refinedSchema(
        type,
        { kind: "ref", id: existing },
        applyRefinement,
      );
    }
    const id = definitions.length;
    definitionIds.set(type.id, id);
    definitions.push(null);
    const properties = checker
      .getPropertiesOfType(type)
      .filter((property) => property.name !== "__brand")
      .sort((left, right) => left.name.localeCompare(right.name))
      .map((property) => {
        const declaration =
          property.valueDeclaration ??
          property.declarations?.[0] ??
          fallbackNode;
        return {
          name: property.name,
          optional: (property.flags & ts.SymbolFlags.Optional) !== 0,
          schema: schemaFor(
            checker.getTypeOfSymbolAtLocation(property, declaration),
          ),
        };
      });
    const stringIndex = checker.getIndexTypeOfType(type, ts.IndexKind.String);
    definitions[id] = {
      kind: "object",
      properties,
      additional: stringIndex === undefined ? null : schemaFor(stringIndex),
    };
    return refinedSchema(type, { kind: "ref", id }, applyRefinement);
  }

  return { definitions, schemaFor };
}

function renderGuard(name) {
  return `export function is${name}(value: unknown): value is ${name} {
  return validate(roots.${name}, value) && guardRefinement("${name}", value);
}`;
}

function render(targets, definitions) {
  const names = targets.map(({ name }) => name);
  const roots = Object.fromEntries(
    targets.map(({ name, schema }) => [name, schema]),
  );
  return `// Generated by packages/contracts/scripts/generate-guards.mjs. Do not edit.

import { guardRefinement } from "./guard-refinements";
import type {
${names.map((name) => `  ${name},`).join("\n")}
} from "./guard-targets";

type Schema = (
  | {
      readonly kind:
        | "any"
        | "never"
        | "undefined"
        | "string"
        | "number"
        | "boolean";
    }
  | { readonly kind: "literal"; readonly value: unknown }
  | { readonly kind: "array"; readonly item: Schema }
  | { readonly kind: "tuple"; readonly items: readonly Schema[] }
  | { readonly kind: "union"; readonly variants: readonly Schema[] }
  | { readonly kind: "ref"; readonly id: number }
) & { readonly refinement?: string };

type ObjectSchema = {
  readonly kind: "object";
  readonly properties: readonly {
    readonly name: string;
    readonly optional: boolean;
    readonly schema: Schema;
  }[];
  readonly additional: Schema | null;
};

// prettier-ignore
const definitions: readonly ObjectSchema[] = ${JSON.stringify(definitions)};
// prettier-ignore
const roots = ${JSON.stringify(roots)} as const satisfies Record<string, Schema>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validateObject(schema: ObjectSchema, value: unknown): boolean {
  if (!isRecord(value)) return false;
  const known = new Set(schema.properties.map((property) => property.name));
  for (const property of schema.properties) {
    if (!(property.name in value)) {
      if (!property.optional) return false;
      continue;
    }
    if (!validate(property.schema, value[property.name])) return false;
  }
  for (const [key, item] of Object.entries(value)) {
    if (known.has(key)) continue;
    if (schema.additional !== null && !validate(schema.additional, item)) {
      return false;
    }
  }
  return true;
}

function validate(schema: Schema, value: unknown): boolean {
  let structurallyValid: boolean;
  switch (schema.kind) {
    case "any":
      structurallyValid = true;
      break;
    case "never":
      structurallyValid = false;
      break;
    case "undefined":
      structurallyValid = value === undefined;
      break;
    case "string":
      structurallyValid = typeof value === "string";
      break;
    case "number":
      structurallyValid = typeof value === "number" && Number.isFinite(value);
      break;
    case "boolean":
      structurallyValid = typeof value === "boolean";
      break;
    case "literal":
      structurallyValid = value === schema.value;
      break;
    case "array":
      structurallyValid =
        Array.isArray(value) &&
        value.every((item) => validate(schema.item, item));
      break;
    case "tuple":
      structurallyValid =
        Array.isArray(value) &&
        value.length === schema.items.length &&
        schema.items.every((item, index) => validate(item, value[index]));
      break;
    case "union":
      structurallyValid = schema.variants.some((variant) =>
        validate(variant, value),
      );
      break;
    case "ref": {
      const definition = definitions[schema.id];
      structurallyValid =
        definition !== undefined && validateObject(definition, value);
      break;
    }
  }
  return (
    structurallyValid &&
    (schema.refinement === undefined ||
      guardRefinement(schema.refinement, value))
  );
}

${names.map(renderGuard).join("\n\n")}
`;
}

async function formatGenerated(source) {
  const config = (await resolveConfig(outputPath)) ?? {};
  return format(source, {
    ...config,
    filepath: outputPath,
  });
}

function reportDrift(current, expected) {
  const currentLines = current.split("\n");
  const expectedLines = expected.split("\n");
  const mismatch = expectedLines.findIndex(
    (line, index) => line !== currentLines[index],
  );
  const line =
    mismatch === -1 ? Math.max(currentLines.length, 1) : mismatch + 1;
  console.error(
    `[contracts-codegen] generated guards are stale at line ${line}.`,
  );
  console.error(
    "[contracts-codegen] Run: pnpm --filter @bowline/contracts codegen",
  );
}

const program = loadProgram();
const checker = program.getTypeChecker();
const targetsSource = program.getSourceFile(targetsPath);
if (targetsSource === undefined) {
  throw new Error(`Type guard targets not found: ${targetsPath}`);
}
const moduleSymbol = checker.getSymbolAtLocation(targetsSource);
if (moduleSymbol === undefined) {
  throw new Error("Could not resolve guard target module");
}
const builder = createSchemaBuilder(checker, targetsSource);
const targets = checker
  .getExportsOfModule(moduleSymbol)
  .map((exported) => {
    const symbol =
      (exported.flags & ts.SymbolFlags.Alias) !== 0
        ? checker.getAliasedSymbol(exported)
        : exported;
    return {
      name: exported.name,
      schema: builder.schemaFor(
        checker.getDeclaredTypeOfSymbol(symbol),
        false,
      ),
    };
  })
  .sort((left, right) => left.name.localeCompare(right.name));
const generated = await formatGenerated(render(targets, builder.definitions));

if (checkOnly) {
  const current = readFileSync(outputPath, "utf8");
  if (current !== generated) {
    reportDrift(current, generated);
    process.exit(1);
  }
} else {
  writeFileSync(outputPath, generated);
}
