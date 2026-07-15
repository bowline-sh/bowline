use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs, io,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{EventId, ProjectId, WorkspaceId},
};
use serde::Serialize;

use crate::{
    env::{EnvLineKind, parse_env_text},
    events::LocalEventError,
    metadata::{MetadataStore, SetupReceiptRecord, default_database_path},
};

use super::{
    PackageManagerIdentity, SetupCommandPlan, SetupInferenceError, SetupReadinessClassification,
    SetupReadinessState, classify_setup_command_result, collect_setup_identity, infer_setup_plan,
    inferred_receipt_key, inferred_recipe_hash, load_setup_recipe, recipe_receipt_key,
    redact_setup_text, redact_setup_text_with_values, setup_receipt_id,
};

mod command;
mod error;
#[cfg(test)]
mod tests;

pub(crate) use bowline_core::status::SetupReceiptState;
use command::*;
pub use error::SetupRunError;

const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSetupOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub approve_setup: bool,
    pub trigger: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSetupOutcome {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub state: ProjectSetupState,
    pub receipt_ids: Vec<String>,
    pub redacted_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectSetupState {
    Hot,
    SetupBlocked,
    NoSetupNeeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnedSetupCandidate {
    pub command: String,
    pub cwd: String,
    pub suggestion: String,
    pub learned: bool,
}

pub fn learned_setup_candidate(
    project_root: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
    command_text: &str,
) -> Result<LearnedSetupCandidate, SetupRunError> {
    let project_root = project_root.as_ref();
    let cwd = cwd.as_ref();
    let relative_cwd = cwd
        .strip_prefix(project_root)
        .map(|path| {
            if path.as_os_str().is_empty() {
                ".".to_string()
            } else {
                path.display().to_string()
            }
        })
        .unwrap_or_else(|_| cwd.display().to_string());
    let redacted = redact_setup_text(command_text);
    Ok(LearnedSetupCandidate {
        suggestion: format!("last successful boot used: `{}`", redacted.text),
        command: redacted.text,
        cwd: relative_cwd,
        learned: true,
    })
}

pub fn run_project_setup(
    options: ProjectSetupOptions,
) -> Result<ProjectSetupOutcome, SetupRunError> {
    let db_path = options
        .db_path
        .clone()
        .map(Ok)
        .unwrap_or_else(default_database_path)?;
    let store = MetadataStore::open(&db_path)?;
    let workspace = store
        .current_workspace()?
        .ok_or(SetupRunError::MissingWorkspace)?;
    let workspace_root = store
        .current_workspace_root()?
        .ok_or(SetupRunError::MissingRoot)
        .map(PathBuf::from)?;
    let project = store
        .current_project_by_path(&options.project_path)?
        .ok_or_else(|| SetupRunError::MissingProject(options.project_path.clone()))?;
    let project_root = validate_project_root(&workspace_root, &project.path)?;

    store.set_project_hot_state(&workspace.id, &project.id, "warming")?;
    let outcome = run_project_setup_root(
        &store,
        &workspace.id,
        &project.id,
        &project.path,
        &project_root,
        &db_path,
        &options,
    );
    match outcome {
        Ok(outcome) => {
            let hot_state = match outcome.state {
                ProjectSetupState::Hot | ProjectSetupState::NoSetupNeeded => "hot",
                ProjectSetupState::SetupBlocked => "setup.blocked",
            };
            store.set_project_hot_state(&workspace.id, &project.id, hot_state)?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = store.set_project_hot_state(&workspace.id, &project.id, "setup.blocked");
            Err(error)
        }
    }
}

fn run_project_setup_root(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    project_root: &Path,
    db_path: &Path,
    options: &ProjectSetupOptions,
) -> Result<ProjectSetupOutcome, SetupRunError> {
    let recipe_path = project_root.join(".bowlinesetup");
    if let Some(recipe_metadata) = setup_recipe_metadata(&recipe_path)? {
        if recipe_metadata.file_type().is_symlink() {
            return Err(SetupRunError::UnsafeWorkspacePath(format!(
                "{project_path}/.bowlinesetup"
            )));
        }
        if !recipe_metadata.is_file() {
            return Err(SetupRunError::UnsafeWorkspacePath(format!(
                "{project_path}/.bowlinesetup"
            )));
        }
        let recipe = load_setup_recipe(project_root, &recipe_path)?;
        if recipe.commands.is_empty() {
            return Ok(ProjectSetupOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: ProjectSetupState::NoSetupNeeded,
                receipt_ids: Vec::new(),
                redacted_summary: "Setup recipe did not contain runnable commands.".to_string(),
            });
        }
        let setup_context = SetupCommandContext {
            store,
            workspace_id,
            project_id,
            project_path,
            project_root,
            trigger: &options.trigger,
            db_path,
            now: &options.generated_at,
        };
        let prior_approved =
            receipt_exists_for_hash(store, workspace_id, project_id, &recipe.recipe_hash)?;
        if !options.approve_setup && !prior_approved {
            let receipt_id = record_approval_required_receipt(ApprovalRequiredReceiptRequest {
                context: setup_context,
                recipe_hash: &recipe.recipe_hash,
                source: ".bowlinesetup",
                command: "recipe approval required",
                redacted_summary: "Setup recipe needs local approval before execution.".to_string(),
                setup_identity_hash: setup_identity_hash_for_root(
                    project_root,
                    Some(recipe.recipe_hash.clone()),
                    None,
                )?,
                event_summary: "Setup recipe needs local approval before execution.",
            })?;
            return Ok(ProjectSetupOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: ProjectSetupState::SetupBlocked,
                receipt_ids: vec![receipt_id],
                redacted_summary: "Setup recipe needs local approval before execution.".to_string(),
            });
        }
        if options.approve_setup {
            record_setup_approval(
                &setup_context,
                SetupApprovalReceipt {
                    recipe_hash: &recipe.recipe_hash,
                    source: ".bowlinesetup",
                },
            )?;
        }

        let mut receipt_ids = Vec::new();
        for command in recipe.commands {
            let receipt_key = recipe_receipt_key(&command, &recipe.recipe_hash)?;
            let expected_receipt_id =
                setup_receipt_id(workspace_id, project_id, &recipe.recipe_hash, &receipt_key);
            if setup_receipt_state(store, workspace_id, &expected_receipt_id)?.is_some_and(
                |state| SetupReceiptState::from_wire(&state) == Some(SetupReceiptState::Completed),
            ) {
                receipt_ids.push(expected_receipt_id);
                continue;
            }
            let receipt_id = run_shell_command(
                SetupCommandContext { ..setup_context },
                SetupShellCommand {
                    command_text: &command.command,
                    receipt_key: &receipt_key,
                    recipe_hash: &recipe.recipe_hash,
                    approval_state: SetupApprovalState::Approved,
                    package_manager: None,
                    cwd: &command.cwd,
                },
            )?;
            receipt_ids.push(receipt_id.clone());
            if store
                .setup_receipt_by_id(workspace_id, &receipt_id)?
                .is_some_and(|receipt| {
                    SetupReceiptState::from_wire(&receipt.state) == Some(SetupReceiptState::Failed)
                })
            {
                return Ok(ProjectSetupOutcome {
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    project_path: project_path.to_string(),
                    state: ProjectSetupState::SetupBlocked,
                    receipt_ids,
                    redacted_summary: "Setup stopped after the first failed command.".to_string(),
                });
            }
        }

        return Ok(ProjectSetupOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: ProjectSetupState::Hot,
            receipt_ids,
            redacted_summary: "Setup completed with redacted output.".to_string(),
        });
    }

    let Some(plan) = infer_setup_plan(project_root)? else {
        return Ok(ProjectSetupOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: ProjectSetupState::NoSetupNeeded,
            receipt_ids: Vec::new(),
            redacted_summary: "No setup recipe or safe lockfile restore was needed.".to_string(),
        });
    };

    if !plan.blockers.is_empty() {
        let setup_context = SetupCommandContext {
            store,
            workspace_id,
            project_id,
            project_path,
            project_root,
            trigger: &options.trigger,
            db_path,
            now: &options.generated_at,
        };
        let summary = plan
            .blockers
            .iter()
            .map(|blocker| blocker.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        let receipt_id = record_approval_required_receipt(ApprovalRequiredReceiptRequest {
            context: setup_context,
            recipe_hash: "inferred:toolchains",
            source: "toolchains",
            command: "toolchain setup blocker",
            redacted_summary: summary.clone(),
            setup_identity_hash: setup_identity_hash_for_root(
                project_root,
                Some("inferred:toolchains".to_string()),
                None,
            )?,
            event_summary: "Toolchain setup needs local action before execution.",
        })?;
        return Ok(ProjectSetupOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: ProjectSetupState::SetupBlocked,
            receipt_ids: vec![receipt_id],
            redacted_summary: summary,
        });
    }

    if plan.commands.is_empty() {
        return Ok(ProjectSetupOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: ProjectSetupState::NoSetupNeeded,
            receipt_ids: Vec::new(),
            redacted_summary: "No setup recipe or safe lockfile restore was needed.".to_string(),
        });
    }

    if plan
        .commands
        .iter()
        .any(|command| command.approval_required)
        && !options.approve_setup
        && !inferred_commands_completed(store, workspace_id, project_id, &plan.commands)?
    {
        let setup_context = SetupCommandContext {
            store,
            workspace_id,
            project_id,
            project_path,
            project_root,
            trigger: &options.trigger,
            db_path,
            now: &options.generated_at,
        };
        let setup_identity_hash = commands_setup_identity_hash(&plan.commands)?;
        let reasons = plan
            .commands
            .iter()
            .flat_map(|command| command.approval_reasons.clone())
            .collect::<Vec<_>>()
            .join("; ");
        let receipt_id = record_approval_required_receipt(ApprovalRequiredReceiptRequest {
            context: setup_context,
            recipe_hash: "inferred",
            source: "inferred",
            command: "inferred setup approval required",
            redacted_summary: format!("Inferred setup needs approval: {reasons}"),
            setup_identity_hash,
            event_summary: "Inferred setup needs local approval before execution.",
        })?;
        return Ok(ProjectSetupOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: ProjectSetupState::SetupBlocked,
            receipt_ids: vec![receipt_id],
            redacted_summary: "Inferred setup needs local approval before execution.".to_string(),
        });
    }
    if options.approve_setup
        && plan
            .commands
            .iter()
            .any(|command| command.approval_required)
    {
        let setup_context = SetupCommandContext {
            store,
            workspace_id,
            project_id,
            project_path,
            project_root,
            trigger: &options.trigger,
            db_path,
            now: &options.generated_at,
        };
        record_setup_approval(
            &setup_context,
            SetupApprovalReceipt {
                recipe_hash: "inferred",
                source: "inferred",
            },
        )?;
    }

    run_inferred_plan(
        store,
        workspace_id,
        project_id,
        project_path,
        plan.commands,
        db_path,
        options,
    )
}

struct ApprovalRequiredReceiptRequest<'a> {
    context: SetupCommandContext<'a>,
    recipe_hash: &'a str,
    source: &'a str,
    command: &'a str,
    redacted_summary: String,
    setup_identity_hash: String,
    event_summary: &'a str,
}

fn record_approval_required_receipt(
    request: ApprovalRequiredReceiptRequest<'_>,
) -> Result<String, SetupRunError> {
    let receipt_id = setup_receipt_id(
        request.context.workspace_id,
        request.context.project_id,
        request.recipe_hash,
        "approval",
    );
    request
        .context
        .store
        .upsert_setup_receipt(&SetupReceiptRecord {
            id: receipt_id.clone(),
            workspace_id: request.context.workspace_id.clone(),
            project_id: Some(request.context.project_id.clone()),
            command: request.command.to_string(),
            state: SetupReceiptState::ApprovalRequired.as_str().to_string(),
            recipe_hash: request.recipe_hash.to_string(),
            approval_state: SetupApprovalState::Required.as_str().to_string(),
            trigger: request.context.trigger.to_string(),
            cwd: request.context.project_path.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            env_profile: "default".to_string(),
            output_path: None,
            redacted_summary: request.redacted_summary.clone(),
            setup_identity_hash: request.setup_identity_hash,
            readiness_state: SetupReadinessState::NeedsSetup.as_str().to_string(),
            readiness_reason: request.redacted_summary,
            readiness_remedy: "Approve setup locally, then rerun setup for the hot project."
                .to_string(),
            receipt_json: serde_json::to_string(&ApprovalReceipt {
                recipe_hash: request.recipe_hash,
                source: request.source,
                trigger: request.context.trigger,
            })?,
            updated_at: request.context.now.to_string(),
        })?;
    append_setup_event(
        &request.context,
        SetupEventRecord {
            name: EventName::SetupBlocked,
            severity: EventSeverity::Attention,
            summary: request.event_summary,
            receipt_id: &receipt_id,
        },
    )?;
    Ok(receipt_id)
}

fn run_inferred_plan(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    commands: Vec<SetupCommandPlan>,
    db_path: &Path,
    options: &ProjectSetupOptions,
) -> Result<ProjectSetupOutcome, SetupRunError> {
    let mut receipt_ids = Vec::new();
    let inferred_project_root = commands
        .first()
        .map(|command| command.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let setup_context = SetupCommandContext {
        store,
        workspace_id,
        project_id,
        project_path,
        project_root: &inferred_project_root,
        trigger: &options.trigger,
        db_path,
        now: &options.generated_at,
    };
    for command in commands {
        let command_text = command.command.join(" ");
        let recipe_hash = inferred_recipe_hash(&command);
        let receipt_key = inferred_receipt_key(&command, &command_text)?;
        let expected_receipt_id =
            setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key);
        if setup_receipt_state(store, workspace_id, &expected_receipt_id)?.is_some_and(|state| {
            SetupReceiptState::from_wire(&state) == Some(SetupReceiptState::Completed)
        }) {
            receipt_ids.push(expected_receipt_id);
            continue;
        }
        let receipt_id = run_shell_command(
            SetupCommandContext { ..setup_context },
            SetupShellCommand {
                command_text: &command_text,
                receipt_key: &receipt_key,
                recipe_hash: &recipe_hash,
                approval_state: if command.approval_required {
                    SetupApprovalState::Approved
                } else {
                    SetupApprovalState::NotRequired
                },
                package_manager: Some(&command.package_manager),
                cwd: &command.cwd,
            },
        )?;
        receipt_ids.push(receipt_id.clone());
        if store
            .setup_receipt_by_id(workspace_id, &receipt_id)?
            .is_some_and(|receipt| {
                SetupReceiptState::from_wire(&receipt.state) == Some(SetupReceiptState::Failed)
            })
        {
            return Ok(ProjectSetupOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: ProjectSetupState::SetupBlocked,
                receipt_ids,
                redacted_summary: "Setup stopped after the first failed command.".to_string(),
            });
        }
    }
    Ok(ProjectSetupOutcome {
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        project_path: project_path.to_string(),
        state: ProjectSetupState::Hot,
        receipt_ids,
        redacted_summary: "Setup completed with redacted output.".to_string(),
    })
}

fn inferred_commands_completed(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    commands: &[SetupCommandPlan],
) -> Result<bool, SetupRunError> {
    if commands.is_empty() {
        return Ok(false);
    }
    for command in commands {
        let command_text = command.command.join(" ");
        let recipe_hash = inferred_recipe_hash(command);
        let receipt_key = inferred_receipt_key(command, &command_text)?;
        let receipt_id = setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key);
        if setup_receipt_state(store, workspace_id, &receipt_id)?.is_none_or(|state| {
            SetupReceiptState::from_wire(&state) != Some(SetupReceiptState::Completed)
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_project_root(
    workspace_root: &Path,
    project_path: &str,
) -> Result<PathBuf, SetupRunError> {
    let relative = Path::new(project_path);
    let mut checked_root = workspace_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(SetupRunError::UnsafeWorkspacePath(project_path.to_string()));
        };
        checked_root.push(segment);
        match fs::symlink_metadata(&checked_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SetupRunError::UnsafeWorkspacePath(project_path.to_string()));
            }
            Ok(_) => {}
            Err(error) => return Err(SetupRunError::Io(error)),
        }
    }

    let accepted_root = fs::canonicalize(workspace_root)?;
    let canonical_project_root = fs::canonicalize(&checked_root)?;
    if canonical_project_root.starts_with(&accepted_root) {
        Ok(checked_root)
    } else {
        Err(SetupRunError::UnsafeWorkspacePath(project_path.to_string()))
    }
}

pub(crate) fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(stdout.len() + stderr.len() + 1);
    bytes.extend_from_slice(stdout);
    if !stderr.is_empty() {
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(stderr);
    }
    String::from_utf8_lossy(&bytes).to_string()
}

pub(crate) fn bounded_output_text(text: &str) -> String {
    if text.len() <= MAX_CAPTURED_OUTPUT {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .take_while(|(index, _)| *index <= MAX_CAPTURED_OUTPUT)
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0)
        .min(text.len());
    let mut output = text[..end].to_string();
    output.push_str("\n[bowline output truncated]\n");
    output
}

fn setup_identity_hash_for_root(
    project_root: &Path,
    recipe_hash: Option<String>,
    package_manager: Option<PackageManagerIdentity>,
) -> Result<String, SetupRunError> {
    Ok(collect_setup_identity(project_root, "default", recipe_hash, package_manager)?.hash)
}

fn commands_setup_identity_hash(commands: &[SetupCommandPlan]) -> Result<String, SetupRunError> {
    if let Some(command) = commands.first() {
        return setup_identity_hash_for_root(
            &command.cwd,
            Some(inferred_recipe_hash(command)),
            Some(command.package_manager.clone()),
        );
    }
    Err(SetupRunError::EmptySetupPlan)
}

pub(crate) fn known_env_values(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<Vec<String>, SetupRunError> {
    let env_sources = store
        .env_records(workspace_id)?
        .into_iter()
        .map(|record| record.source_path)
        .collect::<BTreeSet<_>>();
    let mut values = BTreeSet::new();
    for source in &env_sources {
        collect_env_values_from_file(source, &workspace_root.join(source), &mut values)?;
    }
    let mut directory = Some(cwd);
    while let Some(current) = directory {
        if !current.starts_with(workspace_root) {
            break;
        }
        collect_env_values_from_dir(current, &mut values)?;
        if current == workspace_root {
            break;
        }
        directory = current.parent();
    }
    Ok(values.into_iter().collect())
}

fn collect_env_values_from_dir(
    directory: &Path,
    values: &mut BTreeSet<String>,
) -> Result<(), SetupRunError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_env_file_name(&name) || !entry.file_type()?.is_file() {
            continue;
        }
        collect_env_values_from_file(name.as_ref(), &entry.path(), values)?;
    }
    Ok(())
}

fn collect_env_values_from_file(
    source_path: &str,
    path: &Path,
    values: &mut BTreeSet<String>,
) -> Result<(), SetupRunError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(SetupRunError::Io(error)),
    };
    let parsed = parse_env_text(source_path, "setup", &bytes);
    for line in parsed.lines {
        if let EnvLineKind::KeyValue(value) = line.kind
            && let Ok(text) = std::str::from_utf8(value.value.as_bytes())
        {
            values.insert(text.to_string());
        }
    }
    Ok(())
}

fn is_env_file_name(name: &str) -> bool {
    crate::policy::is_project_env_name(name)
}

pub(crate) fn write_setup_log(db_path: &Path, receipt_id: &str, text: &str) -> io::Result<PathBuf> {
    let log_dir = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("setup-logs");
    fs::create_dir_all(&log_dir)?;
    let path = log_dir.join(format!("{receipt_id}.log"));
    write_owner_only(&path, text.as_bytes())?;
    Ok(path)
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    remove_file_if_present(path)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    remove_file_if_present(path)?;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(bytes)
        })
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn receipt_exists_for_hash(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    recipe_hash: &str,
) -> Result<bool, SetupRunError> {
    Ok(store
        .setup_receipt_for_recipe(
            workspace_id,
            project_id,
            recipe_hash,
            &[
                SetupReceiptState::Completed.as_str(),
                SetupReceiptState::Approved.as_str(),
            ],
        )?
        .is_some())
}

fn setup_receipt_state(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    receipt_id: &str,
) -> Result<Option<String>, SetupRunError> {
    Ok(store
        .setup_receipt_by_id(workspace_id, receipt_id)?
        .map(|receipt| receipt.state))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalReceipt<'a> {
    recipe_hash: &'a str,
    source: &'a str,
    trigger: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandReceipt {
    command: String,
    identity: super::SetupReceiptIdentityInputs,
    redaction_rules: Vec<String>,
}
