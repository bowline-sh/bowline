import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Single owner of the "am I the process entrypoint?" check every CLI script uses
// to guard its top-level `main()`. Both sides are realpath-normalized before the
// compare: from a symlinked checkout `process.argv[1]` is the symlink path while
// `import.meta.url` resolves to the real file, so a naive URL/string compare
// silently returns false and the CLI never runs (deploy then exits 0 in silence).
// Normalizing both sides makes the guard symlink-safe. Fails closed to `false`
// when argv[1] is absent (`node -e`) or does not resolve to a real path.
export function isEntrypoint(importMetaUrl) {
  const invoked = process.argv[1];
  if (!invoked) return false;
  try {
    return realpathSync(invoked) === realpathSync(fileURLToPath(importMetaUrl));
  } catch {
    return false;
  }
}
