// The source roots every repo-wide gate walks. One owner: a gate that walked a
// different set would silently stop covering a directory.
export const SOURCE_ROOTS = Object.freeze([
  "apps",
  "crates",
  "docs",
  "examples",
  "infra",
  "packages",
  "plans",
  "scripts",
  "tests",
]);
