use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use bowline_core::policy::{AccessFlag, MaterializationMode, PathClassification};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPolicy {
    rules: Vec<IgnoreRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFacts {
    pub relative_path: String,
    pub is_dir: bool,
    pub byte_len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPolicyDecision {
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub matched_rule: String,
    pub rule_source: String,
    pub risk: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoreRule {
    base: String,
    pattern: String,
    include: bool,
    source: String,
}

const LARGE_FILE_BYTES: u64 = 8 * 1024 * 1024;

impl UserPolicy {
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn load(root: &Path) -> io::Result<Self> {
        let mut rules = Vec::new();
        collect_ignore_rules(root, root, &mut rules)?;
        Ok(Self { rules })
    }

    pub fn load_for_path(root: &Path, relative_path: &str) -> io::Result<Self> {
        let mut rules = Vec::new();
        read_ignore_rules(root, root, &mut rules)?;

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
                read_ignore_rules(root, &directory, &mut rules)?;
            }
        }

        Ok(Self { rules })
    }

    pub fn has_include_below(&self, relative_path: &str) -> bool {
        let relative_path = normalize_relative_path(relative_path);
        self.rules.iter().any(|rule| {
            if !rule.include {
                return false;
            }
            let pattern = full_rule_pattern(rule);
            pattern == relative_path || pattern.starts_with(&format!("{relative_path}/"))
        })
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
    let base = classify_builtin(&path, facts.is_dir, facts.byte_len);

    let Some(rule) = policy.match_rule(&path) else {
        return base;
    };

    if rule.include {
        return include_decision(base, rule);
    }

    if preserves_safety_classification(base.classification) {
        return PathPolicyDecision {
            rule_source: rule.source.clone(),
            matched_rule: format!("{}; safety override kept", rule.pattern),
            ..base
        };
    }

    PathPolicyDecision {
        classification: PathClassification::LocalOnly,
        mode: MaterializationMode::Ignore,
        access: vec![AccessFlag::HumanReadable],
        matched_rule: rule.pattern.clone(),
        rule_source: rule.source.clone(),
        risk: "low".to_string(),
        summary: "Ignored by .bowlineignore; bowline will leave this path local.".to_string(),
    }
}

pub fn explain_path_without_policy(path: impl Into<String>) -> PathPolicyDecision {
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

fn classify_builtin(path: &str, _is_dir: bool, byte_len: Option<u64>) -> PathPolicyDecision {
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let name = parts.last().copied().unwrap_or("");

    if is_materialization_temp_path(&parts) {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            "bowline-materialization-temp",
            "built-in",
            "low",
            "bowline materialization temp files are local-only crash-recovery state.",
        );
    }

    if is_git_transient_path(&parts) {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            "git-transient",
            "built-in",
            "low",
            "Git transient state is local-only; bowline will not sync lock/temp files.",
        );
    }

    if parts.contains(&".git") {
        return decision(
            PathClassification::WorkspaceSync,
            MaterializationMode::EncryptedSync,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            "git-opaque-state",
            "built-in",
            "medium",
            "Git state is treated as opaque encrypted workspace bytes; Git does not drive sync.",
        );
    }

    if parts.contains(&".work") {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            "work-view-namespace",
            "built-in",
            "low",
            "Work-view resolver paths are bowline local overlay views, not canonical workspace source.",
        );
    }

    if is_env_name(name) {
        return decision(
            PathClassification::ProjectEnv,
            MaterializationMode::ProjectEnv,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            "project-env-file",
            "built-in",
            "medium",
            "Project environment file will sync as project env metadata; values are not printed.",
        );
    }

    if is_secret_name(name) {
        return decision(
            PathClassification::SecretLooking,
            MaterializationMode::EncryptedSync,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            "secret-looking-name",
            "built-in",
            "high",
            "Secret-looking file will sync only as encrypted content and stays hidden from agents.",
        );
    }

    if is_dependency_path(&parts) {
        return decision(
            PathClassification::Dependency,
            MaterializationMode::LocalRegenerate,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            "dependency-directory",
            "built-in",
            "low",
            "Dependency output is local-regenerate by default.",
        );
    }

    if is_generated_path(&parts) {
        return decision(
            generated_classification(&parts),
            generated_mode(&parts),
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            "generated-or-cache-directory",
            "built-in",
            "low",
            "Generated/cache output is not part of the canonical workspace sync set.",
        );
    }

    if byte_len.is_some_and(|len| len >= LARGE_FILE_BYTES) {
        return decision(
            PathClassification::LargeFile,
            MaterializationMode::Lazy,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            "large-file-threshold",
            "built-in",
            "low",
            "Large file will be lazy-hydrated instead of eagerly materialized.",
        );
    }

    decision(
        PathClassification::WorkspaceSync,
        MaterializationMode::WorkspaceSync,
        vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        "default-workspace-sync",
        "built-in",
        "low",
        "Source-like workspace path syncs by default.",
    )
}

fn decision(
    classification: PathClassification,
    mode: MaterializationMode,
    access: Vec<AccessFlag>,
    matched_rule: &str,
    rule_source: &str,
    risk: &str,
    summary: &str,
) -> PathPolicyDecision {
    PathPolicyDecision {
        classification,
        mode,
        access,
        matched_rule: matched_rule.to_string(),
        rule_source: rule_source.to_string(),
        risk: risk.to_string(),
        summary: summary.to_string(),
    }
}

fn include_decision(base: PathPolicyDecision, rule: &IgnoreRule) -> PathPolicyDecision {
    if matches!(
        base.classification,
        PathClassification::Generated | PathClassification::Dependency | PathClassification::Cache
    ) {
        return PathPolicyDecision {
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            matched_rule: format!("!{}", rule.pattern),
            rule_source: rule.source.clone(),
            risk: "low".to_string(),
            summary: "Included by .bowlineignore; bowline will sync this path as workspace state."
                .to_string(),
        };
    }

    PathPolicyDecision {
        rule_source: rule.source.clone(),
        matched_rule: format!("!{}", rule.pattern),
        ..base
    }
}

fn collect_ignore_rules(
    root: &Path,
    directory: &Path,
    rules: &mut Vec<IgnoreRule>,
) -> io::Result<()> {
    read_ignore_rules(root, directory, rules)?;

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || is_dependency_name(&name) || is_generated_name(&name) {
            continue;
        }
        collect_ignore_rules(root, &entry.path(), rules)?;
    }

    Ok(())
}

fn read_ignore_rules(root: &Path, directory: &Path, rules: &mut Vec<IgnoreRule>) -> io::Result<()> {
    let ignore_path = directory.join(".bowlineignore");
    let contents = match fs::read_to_string(&ignore_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    let base = relative_to_root(root, directory);
    let source = if base.is_empty() {
        ".bowlineignore".to_string()
    } else {
        format!("{base}/.bowlineignore")
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
            rules.push(IgnoreRule {
                base: base.clone(),
                pattern: normalize_relative_path(pattern),
                include,
                source: source.clone(),
            });
        }
    }

    Ok(())
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
        relative_path.to_string()
    } else {
        let Some(rest) = relative_path.strip_prefix(&format!("{}/", rule.base)) else {
            return false;
        };
        rest.to_string()
    };
    pattern_matches(&rule.pattern, &target)
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

fn pattern_matches(pattern: &str, target: &str) -> bool {
    let directory_only = pattern.ends_with('/');
    let pattern = pattern.trim_matches('/');
    if pattern.is_empty() {
        return false;
    }

    if directory_only {
        return target == pattern || target.starts_with(&format!("{pattern}/"));
    }

    if !pattern.contains('*') {
        return target == pattern || target.starts_with(&format!("{pattern}/"));
    }

    wildcard_matches(pattern, target)
}

fn wildcard_matches(pattern: &str, target: &str) -> bool {
    let mut rest = target;
    let mut anchored_start = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            anchored_start = false;
            continue;
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        if anchored_start && index != 0 {
            return false;
        }
        rest = &rest[index + part.len()..];
        anchored_start = false;
    }

    pattern.ends_with('*') || rest.is_empty()
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

fn preserves_safety_classification(classification: PathClassification) -> bool {
    matches!(
        classification,
        PathClassification::ProjectEnv
            | PathClassification::SecretLooking
            | PathClassification::Blocked
    )
}

fn is_git_transient_path(parts: &[&str]) -> bool {
    if !parts.contains(&".git") {
        return false;
    }
    let path = parts.join("/");
    path.ends_with(".lock") || path.ends_with("/gc.log") || path.contains("/objects/pack/tmp_")
}

fn is_materialization_temp_path(parts: &[&str]) -> bool {
    parts
        .iter()
        .any(|part| part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
}

fn is_env_name(name: &str) -> bool {
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env")
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "id_rsa"
        || lower == "id_dsa"
        || lower == "id_ed25519"
        || lower.contains("private_key")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
}

fn is_dependency_path(parts: &[&str]) -> bool {
    parts.iter().any(|part| is_dependency_name(part))
}

fn is_generated_path(parts: &[&str]) -> bool {
    parts.iter().any(|part| is_generated_name(part))
}

fn is_dependency_name(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".pnpm-store" | ".yarn" | ".venv" | "venv"
    )
}

fn is_generated_name(name: &str) -> bool {
    matches!(
        name,
        ".next"
            | ".nuxt"
            | ".svelte-kit"
            | "dist"
            | "build"
            | "target"
            | "__pycache__"
            | ".pytest_cache"
            | ".turbo"
            | ".cache"
            | "coverage"
            | "out"
    )
}

fn generated_classification(parts: &[&str]) -> PathClassification {
    if parts
        .iter()
        .any(|part| matches!(*part, ".cache" | ".pytest_cache"))
    {
        PathClassification::Cache
    } else {
        PathClassification::Generated
    }
}

fn generated_mode(parts: &[&str]) -> MaterializationMode {
    if parts
        .iter()
        .any(|part| matches!(*part, ".cache" | ".pytest_cache"))
    {
        MaterializationMode::LocalCache
    } else {
        MaterializationMode::LocalRegenerate
    }
}

#[cfg(test)]
mod tests {
    use super::{PathFacts, UserPolicy, classify_path};

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

        assert_eq!(decision.matched_rule, "default-workspace-sync");
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
        assert!(decision.matched_rule.contains("safety override"));
    }

    #[test]
    fn bowlineignore_include_restores_generated_dependency_and_cache_paths() {
        let temp = crate::workspace::TempWorkspace::new("policy-include").expect("temp workspace");
        temp.write_file(
            ".bowlineignore",
            b"node_modules\n!node_modules/kept.js\ndist\n!dist/asset.js\n.cache\n!.cache/index.json\n",
        )
        .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

        for path in ["node_modules/kept.js", "dist/asset.js", ".cache/index.json"] {
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
                "workspace-sync"
            );
            assert_eq!(
                serde_json::to_value(decision.mode).unwrap(),
                "workspace-sync"
            );
            assert!(decision.matched_rule.starts_with('!'));
        }
    }

    #[test]
    fn bowlineignore_include_does_not_restore_git_transients_or_secrets() {
        let temp =
            crate::workspace::TempWorkspace::new("policy-include-safety").expect("temp workspace");
        temp.write_file(".bowlineignore", b"!.git/index.lock\n!id_rsa\n")
            .expect("ignore file");
        let policy = UserPolicy::load(temp.root()).expect("policy loads");

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

        assert_eq!(decision.matched_rule, "bowline-materialization-temp");
        assert_eq!(serde_json::to_value(decision.mode).unwrap(), "local-only");
    }
}
