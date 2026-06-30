use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, ExplainCommandOutput},
    ids::{ProjectId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::SafeAction,
};

use crate::{
    metadata::{
        DatabaseState, LocalPathRecord, MetadataError, MetadataStore, default_database_path,
    },
    policy::{
        PathFacts, PathPolicyDecision, UserPolicy, classify_path, explain_path_without_policy,
    },
};

#[derive(Debug, Clone)]
pub struct ExplainOptions {
    pub db_path: Option<PathBuf>,
    pub requested_path: String,
    pub generated_at: String,
}

#[derive(Debug)]
pub enum LocalExplainError {
    Io(io::Error),
    Metadata(MetadataError),
    MetadataState(DatabaseState),
}

pub fn compose_explain(options: ExplainOptions) -> Result<ExplainCommandOutput, LocalExplainError> {
    let db_path = options
        .db_path
        .clone()
        .map(Ok)
        .unwrap_or_else(default_database_path)?;
    let inspection = MetadataStore::inspect(&db_path);

    match inspection.state {
        DatabaseState::Missing | DatabaseState::Empty => Ok(explain_without_metadata(options)),
        DatabaseState::Corrupt
        | DatabaseState::FutureIncompatible { .. }
        | DatabaseState::UnsupportedSchema
        | DatabaseState::Locked
        | DatabaseState::PermissionDenied => {
            Err(LocalExplainError::MetadataState(inspection.state))
        }
        DatabaseState::Current => {
            let store = MetadataStore::open(&db_path)?;
            explain_from_store(&store, options)
        }
    }
}

pub fn render_explain_human(output: &ExplainCommandOutput) -> String {
    let mut lines = vec![
        format!("Path: {}", output.path),
        format!(
            "Policy: {} / {}",
            variant_label(&output.classification),
            variant_label(&output.mode)
        ),
        format!("Access: {}", access_labels(&output.access).join(", ")),
        format!(
            "Rule: {} ({}, risk: {})",
            output.matched_rule, output.rule_source, output.risk
        ),
        format!("State: {}", output.observed_state),
        format!("Summary: {}", output.summary),
    ];

    if !output.advisory_notes.is_empty() {
        lines.push("Notes:".to_string());
        lines.extend(output.advisory_notes.iter().map(|note| format!("  {note}")));
    }
    if !output.next_actions.is_empty() {
        lines.push("Suggested actions:".to_string());
        lines.extend(
            output
                .next_actions
                .iter()
                .map(|action| match &action.command {
                    Some(command) => format!("  {}: {command}", action.label),
                    None => format!("  {}", action.label),
                }),
        );
    }

    lines.push(String::new());
    lines.join("\n")
}

impl fmt::Display for LocalExplainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "explain I/O failed: {error}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::MetadataState(state) => write!(formatter, "metadata unavailable: {state:?}"),
        }
    }
}

impl Error for LocalExplainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::MetadataState(_) => None,
        }
    }
}

impl From<io::Error> for LocalExplainError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MetadataError> for LocalExplainError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

fn explain_without_metadata(options: ExplainOptions) -> ExplainCommandOutput {
    let decision = explain_path_without_policy(options.requested_path.clone());
    output_from_decision(
        options.generated_at,
        None,
        None,
        options.requested_path,
        decision,
        "metadata-missing",
        vec!["Local metadata is missing; this is a classifier-only explanation.".to_string()],
    )
}

fn explain_from_store(
    store: &MetadataStore,
    options: ExplainOptions,
) -> Result<ExplainCommandOutput, LocalExplainError> {
    let workspace = store.current_workspace()?;
    let workspace_id = workspace.as_ref().map(|record| record.id.clone());
    let project = store.current_project_by_path(&options.requested_path)?;
    let project_id = project.as_ref().map(|record| record.id.clone());

    let Some(workspace_id) = workspace_id else {
        return Ok(explain_without_metadata(options));
    };

    let root = store.current_workspace_root()?;
    let relative_path = store.workspace_relative_path(&workspace_id, &options.requested_path)?;
    let observed_path = store.observed_path(&workspace_id, &options.requested_path)?;
    let observed_state = if observed_path.is_some() {
        "observed"
    } else {
        "not-observed"
    };
    let decision = match observed_path {
        Some(record) => decision_from_observed_record(record),
        None => decision_for_path(root.as_deref(), &relative_path)?,
    };
    let mut notes =
        vec!["bowline has observed this workspace read-only; sync has not started.".to_string()];
    if relative_path.split('/').any(|part| part == ".git") {
        notes.push(
            "Git information is advisory only; bowline does not mutate or drive Git.".to_string(),
        );
    }

    Ok(output_from_decision(
        options.generated_at,
        Some(workspace_id),
        project_id,
        display_requested_path(&options.requested_path),
        decision,
        observed_state,
        notes,
    ))
}

fn decision_from_observed_record(record: LocalPathRecord) -> PathPolicyDecision {
    PathPolicyDecision {
        classification: record.classification,
        mode: record.mode,
        access: record.access,
        matched_rule: record.matched_rule,
        rule_source: record.rule_source,
        risk: record.risk,
        summary: record.summary,
    }
}

fn decision_for_path(
    root: Option<&str>,
    relative_path: &str,
) -> Result<PathPolicyDecision, LocalExplainError> {
    let Some(root) = root else {
        return Ok(explain_path_without_policy(relative_path.to_string()));
    };
    let root_path = PathBuf::from(root);
    let absolute_path = root_path.join(relative_path);
    let metadata = fs::symlink_metadata(&absolute_path).ok();
    let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
    let byte_len = metadata.as_ref().and_then(|metadata| {
        if metadata.is_dir() {
            None
        } else {
            Some(metadata.len())
        }
    });
    let policy = UserPolicy::load_for_path(&root_path, relative_path)?;

    Ok(classify_path(
        &PathFacts {
            relative_path: relative_path.to_string(),
            is_dir,
            byte_len,
        },
        &policy,
    ))
}

fn output_from_decision(
    generated_at: String,
    workspace_id: Option<WorkspaceId>,
    project_id: Option<ProjectId>,
    path: String,
    decision: PathPolicyDecision,
    observed_state: &str,
    advisory_notes: Vec<String>,
) -> ExplainCommandOutput {
    let next_actions = next_actions_for_decision(
        observed_state,
        decision.classification,
        decision.mode,
        &decision.access,
    );

    ExplainCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Explain,
        generated_at,
        workspace_id,
        project_id,
        path,
        classification: decision.classification,
        mode: decision.mode,
        access: decision.access,
        matched_rule: decision.matched_rule,
        rule_source: decision.rule_source,
        risk: decision.risk,
        observed_state: observed_state.to_string(),
        advisory_notes,
        summary: decision.summary,
        next_actions,
    }
}

fn next_actions_for_decision(
    observed_state: &str,
    classification: PathClassification,
    mode: MaterializationMode,
    access: &[AccessFlag],
) -> Vec<SafeAction> {
    if observed_state == "metadata-missing" {
        return vec![SafeAction {
            label: "Inspect local metadata".to_string(),
            command: Some("bowline status".to_string()),
        }];
    }

    if observed_state == "not-observed" {
        return vec![SafeAction {
            label: "Refresh workspace observation".to_string(),
            command: Some("bowline status".to_string()),
        }];
    }

    if access.contains(&AccessFlag::AgentHidden) {
        return vec![SafeAction {
            label: "Ask before agent access".to_string(),
            command: None,
        }];
    }

    if matches!(
        classification,
        PathClassification::Generated
            | PathClassification::Dependency
            | PathClassification::Cache
            | PathClassification::LocalOnly
    ) || matches!(
        mode,
        MaterializationMode::LocalRegenerate
            | MaterializationMode::LocalCache
            | MaterializationMode::Ignore
            | MaterializationMode::LocalOnly
    ) {
        return vec![SafeAction {
            label: "Review path policy".to_string(),
            command: None,
        }];
    }

    vec![SafeAction {
        label: "Inspect workspace status".to_string(),
        command: Some("bowline status".to_string()),
    }]
}

fn display_requested_path(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        return path.to_string();
    }
    let path = Path::new(path);
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

fn variant_label<T>(value: &T) -> String
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn access_labels(access: &[AccessFlag]) -> Vec<String> {
    access.iter().map(variant_label).collect()
}

#[cfg(test)]
mod tests {
    use crate::{
        explain::{ExplainOptions, compose_explain},
        init::{InitOptions, initialize_root},
        workspace::TempWorkspace,
    };

    #[test]
    fn explain_reports_observed_env_policy() {
        let temp = TempWorkspace::new("explain-env").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        let env_path = temp
            .write_project_file("apps/web", ".env.local", b"API_KEY=value\n")
            .expect("env file");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        initialize_root(InitOptions {
            db_path: Some(db_path.clone()),
            requested_root: Some(temp.root().display().to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        })
        .expect("init succeeds");

        let output = compose_explain(ExplainOptions {
            db_path: Some(db_path),
            requested_path: env_path.display().to_string(),
            generated_at: "2026-06-24T12:00:01Z".to_string(),
        })
        .expect("explain succeeds");

        assert_eq!(serde_json::to_value(output.mode).unwrap(), "project-env");
        assert_eq!(output.observed_state, "observed");
        assert!(!output.summary.contains("API_KEY"));
    }

    #[test]
    fn explain_uses_observed_policy_even_after_file_changes() {
        let temp = TempWorkspace::new("explain-observed-stable").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        let large_path = temp
            .write_project_file("apps/web", "large.bin", &vec![1; 8 * 1024 * 1024 + 1])
            .expect("large file");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        initialize_root(InitOptions {
            db_path: Some(db_path.clone()),
            requested_root: Some(temp.root().display().to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        })
        .expect("init succeeds");
        std::fs::write(&large_path, b"small").expect("shrink file");

        let output = compose_explain(ExplainOptions {
            db_path: Some(db_path),
            requested_path: large_path.display().to_string(),
            generated_at: "2026-06-24T12:00:01Z".to_string(),
        })
        .expect("explain succeeds");

        assert_eq!(serde_json::to_value(output.mode).unwrap(), "lazy");
        assert_eq!(output.observed_state, "observed");
        assert_eq!(output.matched_rule, "large-file-threshold");
    }

    #[test]
    fn explain_reports_git_index_as_opaque_encrypted_state() {
        let temp = TempWorkspace::new("explain-git-index").expect("temp workspace");
        temp.write_file("repo/.git/HEAD", b"ref: refs/heads/main\n")
            .expect("head");
        let index_path = temp
            .write_file("repo/.git/index", b"opaque git index bytes")
            .expect("index");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        initialize_root(InitOptions {
            db_path: Some(db_path.clone()),
            requested_root: Some(temp.root().display().to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        })
        .expect("init succeeds");

        let output = compose_explain(ExplainOptions {
            db_path: Some(db_path),
            requested_path: index_path.display().to_string(),
            generated_at: "2026-06-24T12:00:01Z".to_string(),
        })
        .expect("explain succeeds");

        assert_eq!(serde_json::to_value(output.mode).unwrap(), "encrypted-sync");
        assert_eq!(
            serde_json::to_value(output.classification).unwrap(),
            "workspace-sync"
        );
        assert_eq!(output.matched_rule, "git-opaque-state");
        assert_eq!(output.observed_state, "observed");
        assert!(output.summary.contains("opaque encrypted workspace bytes"));
        assert!(output.advisory_notes.iter().any(|note| {
            note.contains("advisory only") && note.contains("does not mutate or drive Git")
        }));
    }

    #[test]
    fn explain_reports_git_index_lock_as_local_only_transient() {
        let temp = TempWorkspace::new("explain-git-index-lock").expect("temp workspace");
        temp.write_file("repo/.git/HEAD", b"ref: refs/heads/main\n")
            .expect("head");
        let lock_path = temp
            .write_file("repo/.git/index.lock", b"lock")
            .expect("index lock");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        initialize_root(InitOptions {
            db_path: Some(db_path.clone()),
            requested_root: Some(temp.root().display().to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        })
        .expect("init succeeds");

        let output = compose_explain(ExplainOptions {
            db_path: Some(db_path),
            requested_path: lock_path.display().to_string(),
            generated_at: "2026-06-24T12:00:01Z".to_string(),
        })
        .expect("explain succeeds");

        assert_eq!(serde_json::to_value(output.mode).unwrap(), "local-only");
        assert_eq!(
            serde_json::to_value(output.classification).unwrap(),
            "local-only"
        );
        assert_eq!(output.observed_state, "observed");
        assert!(output.summary.contains("Git transient"));
        assert!(output.advisory_notes.iter().any(|note| {
            note.contains("advisory only") && note.contains("does not mutate or drive Git")
        }));
    }
}
