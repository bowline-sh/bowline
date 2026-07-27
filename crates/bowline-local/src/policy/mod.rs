use std::{
    collections::BTreeSet,
    fs, io,
    path::{Component, Path, PathBuf},
};

use bowline_core::policy::{AccessFlag, MaterializationMode, PathClassification};

use crate::glob::glob_matches;

mod builtin_classification;
mod project_view;
mod state_paths;
mod traversal;
mod types;

pub(crate) use builtin_classification::is_project_env_name;
use builtin_classification::{
    NameHeuristics, classify_builtin, is_dependency_name, is_generated_name,
    preserves_safety_classification,
};
pub(crate) use project_view::classify_project_view_path;
pub use project_view::is_work_view_namespace_path;
pub use state_paths::{is_private_workspace_state_path, is_secret_bearing_path};
pub use traversal::policy_should_recurse;
pub(crate) use traversal::{policy_prunes_subtree, policy_syncs_workspace_state};
pub use types::{PathFacts, PathPolicyDecision};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPolicy {
    rules: Vec<IgnoreRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoreRule {
    base: String,
    pattern: String,
    anchored_to_base: bool,
    include: bool,
    source: String,
    /// Glob variants this rule matches against, expanded once at parse time.
    /// `classify_path` runs per entry of every scan, so the variants must not be
    /// rebuilt with `format!` on each call.
    candidates: Vec<String>,
}

/// The workspace-root policy marker whose contents govern deep include/exclude
/// classification for the whole tree. The single source of truth for the marker
/// filename; `read_ignore_rules` and the daemon dirty-scope guard both route
/// through it rather than repeating the literal.
pub const POLICY_MARKER_FILENAME: &str = ".bowlineignore";

/// Whether a root-level entry's leaf name is a policy input that governs deep
/// include/exclude classification.
///
/// A root-shallow scan reuses preserved deep head entries verbatim, so an edit,
/// deletion, or rename-away of such a file would leave the deep classification
/// stale. Callers must force a full rescan instead of a shallow/scoped pass when
/// this returns true. `.bowlineignore` is currently the only root policy input:
/// the loader (`read_ignore_rules`) reads no other root-level policy file, and
/// built-in classification keys off each path's own name, not sibling root
/// files.
pub fn is_root_policy_affecting_path(leaf: &str) -> bool {
    leaf == POLICY_MARKER_FILENAME
}

impl UserPolicy {
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn load(root: &Path) -> io::Result<Self> {
        let mut rules = Vec::new();
        collect_ignore_rules(root, root, &mut rules, &|_| true);
        Ok(Self { rules })
    }

    /// Load only the workspace root's policy inputs, without walking the tree.
    ///
    /// Root-shallow ticks must not restat `~/Code` for policy discovery, so this
    /// reads the root `.bowlineignore` alone. Deeper `.bowlineignore` files under
    /// dirty roots are picked up by scoped loads ([`Self::load_for_path`]) or a
    /// full [`Self::load`].
    pub fn load_root_only(root: &Path) -> io::Result<Self> {
        let mut rules = Vec::new();
        read_ignore_rules(root, root, &mut rules);
        Ok(Self { rules })
    }

    pub fn load_for_path(root: &Path, relative_path: &str) -> io::Result<Self> {
        let mut rules = Vec::new();
        read_ignore_rules(root, root, &mut rules);

        let Some(parent) = Path::new(relative_path).parent() else {
            return Ok(Self { rules });
        };
        let mut directory = root.to_path_buf();
        for component in parent.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            directory.push(part);
            if directory.is_dir() {
                read_ignore_rules(root, &directory, &mut rules);
            }
        }

        Ok(Self { rules })
    }

    /// Load policy for a scoped subtree scan without walking the whole tree.
    ///
    /// A `DirtySubtrees` tick only rescans `dirty_roots`, so it must not restat
    /// all of `~/Code` for `.bowlineignore` discovery (the O(workspace) traversal
    /// Plan 06 set out to eliminate). This runs the same pre-order DFS as
    /// [`Self::load`] but prunes recursion to directories that lead to or lie
    /// inside a dirty root, so the resulting rule vector is byte-identical in
    /// ordering to a full load restricted to those subtrees — critical because
    /// `match_rule` is last-match-wins and a reordered ancestor rule would flip a
    /// deeper `!`-negation. An empty root means whole-workspace scope, so fall
    /// back to full discovery.
    pub fn load_scoped(root: &Path, dirty_roots: &BTreeSet<String>) -> io::Result<Self> {
        if dirty_roots.iter().any(|dirty| dirty.is_empty()) {
            return Self::load(root);
        }
        let mut rules = Vec::new();
        collect_ignore_rules(root, root, &mut rules, &|relative| {
            dir_leads_to_or_under_dirty(relative, dirty_roots)
        });
        Ok(Self { rules })
    }

    pub fn has_include_below(&self, relative_path: &str) -> bool {
        let relative_path = normalize_relative_path(relative_path);
        self.rules.iter().any(|rule| {
            if !rule.include {
                return false;
            }
            if !rule.anchored_to_base {
                // Slash-free includes match at any depth below their policy
                // file, so every excluded directory may contain a later include.
                return true;
            }
            let pattern = full_rule_pattern(rule);
            pattern == relative_path || pattern.starts_with(&format!("{relative_path}/"))
        })
    }
    pub fn explicitly_includes(&self, relative_path: &str) -> bool {
        self.match_rule(&normalize_relative_path(relative_path))
            .is_some_and(|rule| rule.include)
    }
    fn match_rule(&self, relative_path: &str) -> Option<&IgnoreRule> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule_matches(rule, relative_path))
    }
}

pub fn classify_path(facts: &PathFacts, policy: &UserPolicy) -> PathPolicyDecision {
    let path = normalize_relative_path(&facts.relative_path);
    let base = classify_builtin(&path, facts.is_dir, facts.byte_len, NameHeuristics::Applied);

    let Some(rule) = policy.match_rule(&path) else {
        return base;
    };

    // A `!`-negation is the only escape hatch from the built-in name lists,
    // which match `dist`/`build`/`out`/`target` in *any* path component and
    // would otherwise permanently drop real source such as `docs/build/guide.md`
    // with no way to say otherwise. Only the dependency/generated ancestry is
    // waived: env, secret, git-state and large-file handling still apply, so an
    // include can restore a path without also downgrading how it is treated.
    if rule.include {
        return classify_builtin(&path, facts.is_dir, facts.byte_len, NameHeuristics::Waived);
    }

    if preserves_safety_classification(base.classification) {
        return base;
    }

    PathPolicyDecision {
        classification: PathClassification::LocalOnly,
        mode: MaterializationMode::Ignore,
        access: vec![AccessFlag::HumanReadable],
    }
}

/// Classify a path using only the built-in rules (no `.bowlineignore` policy).
/// Test-only helper for synthesizing scan observations without touching a
/// filesystem-backed `UserPolicy`.
#[cfg(test)]
pub(crate) fn classify_path_with_builtin_policy(path: impl Into<String>) -> PathPolicyDecision {
    let path = path.into();
    classify_path(
        &PathFacts {
            relative_path: path,
            is_dir: false,
            byte_len: None,
        },
        &UserPolicy::empty(),
    )
}

fn collect_ignore_rules(
    root: &Path,
    directory: &Path,
    rules: &mut Vec<IgnoreRule>,
    should_descend: &dyn Fn(&str) -> bool,
) {
    read_ignore_rules(root, directory, rules);

    // A directory whose listing the filesystem refuses contributes no policy and
    // is not descended. It must NOT fail the load: policy discovery runs inside
    // the engine's full scan, so propagating one unreadable directory reached
    // `CycleError::Fatal` and — because every startup stat-walks — failed every
    // start until the user found and fixed the permission. The same directory is
    // recorded unsyncable by the stat walk, so nothing under it is published on
    // the strength of the rules this skipped.
    let Ok(entries) = crate::fs_access::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || is_dependency_name(&name) || is_generated_name(&name) {
            continue;
        }
        let child = entry.path();
        // `should_descend` prunes the recursion for scoped loads. Pre-order DFS
        // means a parent's rules are always appended before a child's, which
        // `match_rule` relies on (last match wins); gating recursion — never
        // reordering — preserves that invariant for the pruned subset.
        if should_descend(&relative_to_root(root, &child)) {
            collect_ignore_rules(root, &child, rules, should_descend);
        }
    }
}

/// Whether a scoped policy load should descend into `relative`: true when it is
/// an ancestor of a dirty root (needed to reach it), equal to one, or inside one
/// (needed for the dirty subtree's own nested `.bowlineignore` files). Anything
/// else is off the dirty frontier and pruned so the walk stays bounded.
fn dir_leads_to_or_under_dirty(relative: &str, dirty_roots: &BTreeSet<String>) -> bool {
    dirty_roots.iter().any(|dirty| {
        dirty == relative
            || dirty.starts_with(&format!("{relative}/"))
            || relative.starts_with(&format!("{dirty}/"))
    })
}

fn read_ignore_rules(root: &Path, directory: &Path, rules: &mut Vec<IgnoreRule>) {
    let ignore_path = directory.join(POLICY_MARKER_FILENAME);
    let contents = match fs::read_to_string(&ignore_path) {
        Ok(contents) => contents,
        // Absent, unreadable, or not valid UTF-8: this directory contributes no
        // rules. Same reasoning as the listing failure in `collect_ignore_rules`
        // — a policy load that fails stops the engine's whole scan, and it stops
        // it again on every restart.
        Err(_) => return,
    };

    let base = relative_to_root(root, directory);
    let source = if base.is_empty() {
        POLICY_MARKER_FILENAME.to_string()
    } else {
        format!("{base}/{POLICY_MARKER_FILENAME}")
    };
    for line in contents.lines() {
        let raw = line.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (include, pattern) = raw
            .strip_prefix('!')
            .map(|pattern| (true, pattern.trim()))
            .unwrap_or((false, raw));
        if !pattern.is_empty() {
            let directory_only = pattern.ends_with('/');
            let leading_slash = pattern.starts_with('/');
            let normalized_pattern = normalize_relative_path(pattern);
            let anchored_to_base = leading_slash || normalized_pattern.contains('/');
            let candidates = matching_patterns(
                normalized_pattern.trim_matches('/'),
                directory_only,
                anchored_to_base,
            );
            rules.push(IgnoreRule {
                base: base.clone(),
                pattern: normalized_pattern,
                anchored_to_base,
                include,
                source: source.clone(),
                candidates,
            });
        }
    }
}

fn relative_to_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(path_to_slash_string)
        .unwrap_or_default()
}

fn path_to_slash_string(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn rule_matches(rule: &IgnoreRule, relative_path: &str) -> bool {
    let target = if rule.base.is_empty() {
        relative_path
    } else {
        match relative_path
            .strip_prefix(rule.base.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(rest) => rest,
            None => return false,
        }
    };
    rule.candidates
        .iter()
        .any(|candidate| glob_matches(candidate, target))
}

fn full_rule_pattern(rule: &IgnoreRule) -> String {
    if rule.base.is_empty() {
        rule.pattern.trim_matches('/').to_string()
    } else {
        format!(
            "{}/{}",
            rule.base.trim_matches('/'),
            rule.pattern.trim_matches('/')
        )
    }
}

fn matching_patterns(pattern: &str, directory_only: bool, anchored_to_base: bool) -> Vec<String> {
    if pattern.is_empty() {
        return Vec::new();
    }
    if anchored_to_base {
        if directory_only {
            return vec![pattern.to_string(), format!("{pattern}/**")];
        }
        return vec![pattern.to_string()];
    }

    if directory_only {
        return vec![
            pattern.to_string(),
            format!("{pattern}/**"),
            format!("**/{pattern}"),
            format!("**/{pattern}/**"),
        ];
    }

    vec![pattern.to_string(), format!("**/{pattern}")]
}
fn normalize_relative_path(path: &str) -> String {
    let mut normalized = PathBuf::from(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        POLICY_MARKER_FILENAME, PathFacts, UserPolicy, classify_path, is_root_policy_affecting_path,
    };

    #[test]
    fn only_bowlineignore_is_a_root_policy_affecting_path() {
        // The loader reads no root-level policy file other than the marker, so
        // the predicate must accept exactly `.bowlineignore` and reject any
        // other root leaf (including manifests and other dotfiles).
        assert_eq!(POLICY_MARKER_FILENAME, ".bowlineignore");
        assert!(is_root_policy_affecting_path(".bowlineignore"));
        assert!(!is_root_policy_affecting_path("README.md"));
        assert!(!is_root_policy_affecting_path("package.json"));
        assert!(!is_root_policy_affecting_path("Cargo.toml"));
        assert!(!is_root_policy_affecting_path(".gitignore"));
        assert!(!is_root_policy_affecting_path(".env"));
        // Only the bare root leaf matters; a nested path is not a *root* input.
        assert!(!is_root_policy_affecting_path("src/.bowlineignore"));
    }

    #[test]
    fn classifies_source_as_workspace_sync() {
        let decision = classify_path(
            &PathFacts {
                relative_path: "apps/web/src/index.ts".to_string(),
                is_dir: false,
                byte_len: Some(1024),
            },
            &UserPolicy::empty(),
        );

        assert_eq!(
            serde_json::to_value(decision.classification).unwrap(),
            "workspace-sync"
        );
        assert_eq!(
            serde_json::to_value(decision.mode).unwrap(),
            "workspace-sync"
        );
    }

    #[test]
    fn bowlineignore_does_not_downgrade_env_files() {
        let temp = crate::workspace::TempWorkspace::new("policy-env").expect("temp workspace");
        temp.write_file(".bowlineignore", b".env.local\n")
            .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        let decision = classify_path(
            &PathFacts {
                relative_path: ".env.local".to_string(),
                is_dir: false,
                byte_len: Some(12),
            },
            &policy,
        );

        assert_eq!(serde_json::to_value(decision.mode).unwrap(), "project-env");
        assert_eq!(
            serde_json::to_value(decision.classification).unwrap(),
            "project-env"
        );
    }

    #[test]
    fn an_explicit_include_overrides_the_built_in_name_heuristics() {
        let temp = crate::workspace::TempWorkspace::new("policy-include").expect("temp workspace");
        temp.write_file(
            ".bowlineignore",
            b"!node_modules/kept.js\n!dist/asset.js\n!.cache/index.json\n!docs/build/guide.md\n",
        )
        .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        for path in [
            "node_modules/kept.js",
            "dist/asset.js",
            ".cache/index.json",
            "docs/build/guide.md",
        ] {
            let decision = classify_path(
                &PathFacts {
                    relative_path: path.to_string(),
                    is_dir: false,
                    byte_len: Some(12),
                },
                &policy,
            );

            assert_eq!(
                serde_json::to_value(decision.classification).unwrap(),
                "workspace-sync",
                "{path}"
            );
            assert_eq!(
                serde_json::to_value(decision.mode).unwrap(),
                "workspace-sync",
                "{path}"
            );
        }
    }

    #[test]
    fn an_explicit_include_does_not_downgrade_env_or_secret_handling() {
        let temp =
            crate::workspace::TempWorkspace::new("policy-include-safety").expect("temp workspace");
        temp.write_file(".bowlineignore", b"!dist/.env\n!dist/server.pem\n")
            .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        // Waiving the ancestry heuristic still lands on the name-based safety
        // classification rather than plain workspace-sync.
        assert_eq!(classification("dist/.env", &policy), "project-env");
        assert_eq!(classification("dist/server.pem", &policy), "secret-looking");
        assert_eq!(classification("dist/asset.js", &policy), "generated");
    }

    #[test]
    fn env_names_are_case_insensitive_but_local_regenerate_ancestry_wins() {
        let policy = UserPolicy::empty();

        for path in [".ENV", ".Env.Local", "production.ENV"] {
            assert_eq!(classification(path, &policy), "project-env", "{path}");
        }
        for path in [
            "node_modules/pkg/.env",
            "target/debug/.ENV.Local",
            "dist/service.env",
            ".cache/tool/.Env",
        ] {
            assert_ne!(classification(path, &policy), "project-env", "{path}");
        }
    }

    #[test]
    fn matching_follows_gitignore_semantics() {
        let temp = crate::workspace::TempWorkspace::new("policy-gitignore").expect("workspace");
        temp.write_file(
            ".bowlineignore",
            b"secret.txt\n*.log\nbuild/\nsrc/gen.rs\n?.rs\n*.js\n!kept.js\nlast.txt\n!last.txt\n!scoped.txt\nscoped/exclude\n",
        )
        .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        // row 1: bare pattern matches at any depth.
        assert_eq!(classification("secret.txt", &policy), "local-only");
        assert_eq!(classification("sub/secret.txt", &policy), "local-only");
        assert_eq!(classification("a/b/secret.txt", &policy), "local-only");
        assert_eq!(classification("secret.txt.bak", &policy), "workspace-sync");

        // row 2: slash-free `*` matches at any depth but never crosses `/`.
        assert_eq!(classification("x.log", &policy), "local-only");
        assert_eq!(classification("deep/y.log", &policy), "local-only");
        assert_eq!(classification("x.log/keep", &policy), "workspace-sync");
        assert_eq!(classification("a.logs", &policy), "workspace-sync");

        // row 3: slash-free trailing-slash directories match at any depth.
        assert_eq!(classification("build", &policy), "generated");
        assert_eq!(classification("build/x", &policy), "generated");
        assert_eq!(classification("sub/build", &policy), "generated");
        assert_eq!(classification("sub/build/x/y", &policy), "generated");
        assert_eq!(classification("rebuild", &policy), "workspace-sync");

        // row 4: slash-containing patterns are anchored to the policy file.
        assert_eq!(classification("src/gen.rs", &policy), "local-only");
        assert_eq!(classification("a/src/gen.rs", &policy), "workspace-sync");
        assert_eq!(classification("src/sub/gen.rs", &policy), "workspace-sync");

        // row 5: anchored `*` stays within one segment.
        let map_temp =
            crate::workspace::TempWorkspace::new("policy-anchored-wildcard").expect("workspace");
        map_temp
            .write_file(".bowlineignore", b"build/*.map\n")
            .expect("ignore file");
        let map_policy = UserPolicy::load(map_temp.root()).expect("policy loads");
        assert_eq!(classification("build/a.map", &map_policy), "generated");
        assert!(map_policy.match_rule("build/sub/a.map").is_none());

        // row 9: `?` matches exactly one non-slash character.
        assert_eq!(classification("a.rs", &policy), "local-only");
        assert_eq!(classification("ab.rs", &policy), "workspace-sync");
        assert_eq!(classification("a/.rs", &policy), "workspace-sync");

        // row 10: include rules use the same any-depth matching semantics.
        assert_eq!(classification("a/b/kept.js", &policy), "workspace-sync");

        // Precedence remains last-match-wins.
        assert_eq!(classification("last.txt", &policy), "workspace-sync");

        let dirty = std::collections::BTreeSet::from(["scoped".to_string()]);
        let scoped = UserPolicy::load_scoped(temp.root(), &dirty).expect("scoped policy");
        assert_eq!(
            classification("scoped/scoped.txt", &scoped),
            "workspace-sync"
        );
    }

    #[test]
    fn double_star_recurses() {
        let temp = crate::workspace::TempWorkspace::new("policy-double-star").expect("workspace");
        temp.write_file(".bowlineignore", b"**/*.min.js\nsrc/**/gen.rs\nlogs/**\n")
            .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        // row 6: leading `**/` matches at any depth.
        assert_eq!(classification("a.min.js", &policy), "local-only");
        assert_eq!(classification("x/y/a.min.js", &policy), "local-only");
        assert_eq!(classification("a.min.jsx", &policy), "workspace-sync");

        // row 7: interior `/**/` matches zero or more segments.
        assert_eq!(classification("src/gen.rs", &policy), "local-only");
        assert_eq!(classification("src/a/b/gen.rs", &policy), "local-only");
        assert_eq!(classification("lib/gen.rs", &policy), "workspace-sync");

        // row 8: trailing `/**` matches contents under the anchored directory.
        assert_eq!(classification("logs/a", &policy), "local-only");
        assert_eq!(classification("logs/a/b", &policy), "local-only");
        assert_eq!(classification("logs", &policy), "workspace-sync");
        assert_eq!(classification("x/logs/a", &policy), "workspace-sync");
    }

    #[test]
    fn load_scoped_matches_full_load_for_sibling_dirty_roots_with_deep_negation() {
        // Regression: two sibling dirty roots sharing an ancestor `.bowlineignore`
        // must not duplicate the ancestor's rules after a deeper `!`-negation.
        // `match_rule` is last-match-wins, so a duplicated ancestor exclude landing
        // after the deep re-include would flip `a/b/secret.log` to local-only and
        // silently stop syncing it (Everything Syncs violation). `load_scoped` must
        // classify identically to a full `load`.
        let temp =
            crate::workspace::TempWorkspace::new("policy-scoped-siblings").expect("workspace");
        temp.write_file("a/.bowlineignore", b"*.log\n")
            .expect("ancestor policy");
        temp.write_file("a/b/.bowlineignore", b"!secret.log\n")
            .expect("deep negation");
        temp.write_file("a/b/secret.log", b"keep me\n")
            .expect("re-included file");
        temp.write_file("a/c/other.rs", b"fn x() {}\n")
            .expect("sibling file");

        let full = UserPolicy::load(temp.root()).expect("full policy");
        let dirty = std::collections::BTreeSet::from(["a/b".to_string(), "a/c".to_string()]);
        let scoped = UserPolicy::load_scoped(temp.root(), &dirty).expect("scoped policy");

        let facts = PathFacts {
            relative_path: "a/b/secret.log".to_string(),
            is_dir: false,
            byte_len: Some(8),
        };
        let full_class = serde_json::to_value(classify_path(&facts, &full).classification).unwrap();
        let scoped_class =
            serde_json::to_value(classify_path(&facts, &scoped).classification).unwrap();
        assert_eq!(
            scoped_class, full_class,
            "scoped policy load diverged from full load for a re-included file"
        );
        assert_eq!(scoped_class, "workspace-sync");
    }

    #[test]
    fn bowlineignore_include_keeps_git_index_opaque_but_not_locks_or_secrets() {
        let temp =
            crate::workspace::TempWorkspace::new("policy-include-safety").expect("temp workspace");
        temp.write_file(
            ".bowlineignore",
            b"!.git/index\n!.git/index.lock\n!id_rsa\n",
        )
        .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        let index = classify_path(
            &PathFacts {
                relative_path: ".git/index".to_string(),
                is_dir: false,
                byte_len: Some(1),
            },
            &policy,
        );
        let lock = classify_path(
            &PathFacts {
                relative_path: ".git/index.lock".to_string(),
                is_dir: false,
                byte_len: Some(1),
            },
            &policy,
        );
        let secret = classify_path(
            &PathFacts {
                relative_path: "id_rsa".to_string(),
                is_dir: false,
                byte_len: Some(1),
            },
            &policy,
        );

        assert_eq!(
            serde_json::to_value(index.classification).unwrap(),
            "workspace-sync"
        );
        assert_eq!(serde_json::to_value(index.mode).unwrap(), "encrypted-sync");
        assert_eq!(serde_json::to_value(lock.mode).unwrap(), "local-only");
        assert_eq!(
            serde_json::to_value(secret.classification).unwrap(),
            "secret-looking"
        );
    }

    #[test]
    fn materialization_temp_files_stay_local_only() {
        let decision = classify_path(
            &PathFacts {
                relative_path: "apps/web/src/.bowline-materialize-index_ts-abcdef123456.tmp"
                    .to_string(),
                is_dir: false,
                byte_len: Some(24),
            },
            &UserPolicy::empty(),
        );

        assert_eq!(
            serde_json::to_value(decision.classification).unwrap(),
            "local-only"
        );
        assert_eq!(serde_json::to_value(decision.mode).unwrap(), "local-only");
    }

    fn classification(path: &str, policy: &UserPolicy) -> String {
        serde_json::to_value(classify_path(&facts(path), policy).classification)
            .expect("classification serializes")
            .as_str()
            .expect("classification is string")
            .to_string()
    }

    fn facts(path: &str) -> PathFacts {
        PathFacts {
            relative_path: path.to_string(),
            is_dir: false,
            byte_len: Some(12),
        }
    }
}
