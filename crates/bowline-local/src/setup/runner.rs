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
    workspace_graph::{HydrationState, NamespaceEntryKind},
};
use serde::Serialize;

use crate::{
    env::{EnvLineKind, parse_env_text},
    events::LocalEventError,
    hydration_budget::reconcile_materialized_hydration_queue,
    metadata::{
        HydrationQueueRecord, MetadataError, MetadataStore, SetupReceiptRecord,
        default_database_path,
    },
};

use super::{
    SetupCommandPlan, SetupInferenceError, collect_receipt_identity_inputs, infer_setup_plan,
    load_setup_recipe, redact_setup_text_with_values,
};

const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrewarmOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub approve_setup: bool,
    pub trigger: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmOutcome {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub state: PrewarmState,
    pub receipt_ids: Vec<String>,
    pub redacted_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrewarmState {
    Hot,
    SetupBlocked,
    NoSetupNeeded,
}

#[derive(Debug)]
pub enum SetupRunError {
    Metadata(MetadataError),
    Io(io::Error),
    Recipe(super::SetupRecipeError),
    Inference(SetupInferenceError),
    UnsafeWorkspacePath(String),
    MissingWorkspace,
    MissingRoot,
    MissingProject(String),
    Json(serde_json::Error),
    Events(LocalEventError),
}

pub fn prewarm_project(options: PrewarmOptions) -> Result<PrewarmOutcome, SetupRunError> {
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
    let outcome = prewarm_project_root(
        &store,
        &workspace.id,
        &project.id,
        &project.path,
        &project_root,
        &db_path,
        &options,
    );
    match outcome {
        Ok(mut outcome) => {
            let queued_prefetch = queue_hot_project_prefetch(
                &store,
                &workspace.id,
                &project.id,
                &options.generated_at,
            )?;
            let completed_prefetch = reconcile_materialized_hydration_queue(
                &store,
                &workspace.id,
                &options.generated_at,
            )?;
            if queued_prefetch > 0 || completed_prefetch > 0 {
                outcome.redacted_summary = format!(
                    "{} Hot project prefetch queued {} file(s); {} already local.",
                    outcome.redacted_summary, queued_prefetch, completed_prefetch
                );
            }
            let hot_state = match outcome.state {
                PrewarmState::Hot | PrewarmState::NoSetupNeeded => "hot",
                PrewarmState::SetupBlocked => "setup.blocked",
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

fn queue_hot_project_prefetch(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    now: &str,
) -> Result<usize, MetadataError> {
    let mut queued = 0;
    for node in store.projected_nodes_for_project(workspace_id, project_id)? {
        if node.kind != NamespaceEntryKind::File || node.hydration_state != HydrationState::Cold {
            continue;
        }
        let Some(content_id) = node.content_id.clone() else {
            continue;
        };
        store.enqueue_hydration(&HydrationQueueRecord {
            id: format!(
                "prefetch_{}",
                blake3::hash(
                    format!(
                        "{}:{}:{}",
                        workspace_id.as_str(),
                        project_id.as_str(),
                        node.path
                    )
                    .as_bytes()
                )
                .to_hex()
            ),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: node.path,
            content_id: Some(content_id),
            priority: "hot-project-prefetch".to_string(),
            state: "queued".to_string(),
            cause: "hot-project-prefetch".to_string(),
            updated_at: now.to_string(),
        })?;
        queued += 1;
    }
    Ok(queued)
}

fn prewarm_project_root(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    project_root: &Path,
    db_path: &Path,
    options: &PrewarmOptions,
) -> Result<PrewarmOutcome, SetupRunError> {
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
            return Ok(PrewarmOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: PrewarmState::NoSetupNeeded,
                receipt_ids: Vec::new(),
                redacted_summary: "Setup recipe did not contain runnable commands.".to_string(),
            });
        }
        let prior_approved =
            receipt_exists_for_hash(store, workspace_id, project_id, &recipe.recipe_hash)?;
        if !options.approve_setup && !prior_approved {
            let receipt_id =
                setup_receipt_id(workspace_id, project_id, &recipe.recipe_hash, "approval");
            store.upsert_setup_receipt(&SetupReceiptRecord {
                id: receipt_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                command: "recipe approval required".to_string(),
                state: "approval-required".to_string(),
                recipe_hash: recipe.recipe_hash.clone(),
                approval_state: "required".to_string(),
                trigger: options.trigger.clone(),
                cwd: project_path.to_string(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                env_profile: "default".to_string(),
                output_path: None,
                redacted_summary: "Setup recipe needs local approval before execution.".to_string(),
                receipt_json: serde_json::to_string(&ApprovalReceipt {
                    recipe_hash: &recipe.recipe_hash,
                    source: ".bowlinesetup",
                    trigger: &options.trigger,
                })?,
                updated_at: options.generated_at.clone(),
            })?;
            append_setup_event(
                store,
                EventName::SetupBlocked,
                EventSeverity::Attention,
                "Setup recipe needs local approval before execution.",
                workspace_id,
                project_id,
                project_path,
                &receipt_id,
                &options.trigger,
                &options.generated_at,
            )?;
            return Ok(PrewarmOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: PrewarmState::SetupBlocked,
                receipt_ids: vec![receipt_id],
                redacted_summary: "Setup recipe needs local approval before execution.".to_string(),
            });
        }
        if options.approve_setup {
            record_setup_approval(
                store,
                workspace_id,
                project_id,
                project_path,
                &recipe.recipe_hash,
                ".bowlinesetup",
                &options.trigger,
                &options.generated_at,
            )?;
        }

        let mut receipt_ids = Vec::new();
        for command in recipe.commands {
            let receipt_key = recipe_receipt_key(&command, &recipe.recipe_hash)?;
            let expected_receipt_id =
                setup_receipt_id(workspace_id, project_id, &recipe.recipe_hash, &receipt_key);
            if setup_receipt_state(store, workspace_id, &expected_receipt_id)?
                .is_some_and(|state| state == "completed")
            {
                receipt_ids.push(expected_receipt_id);
                continue;
            }
            let receipt_id = run_shell_command(
                store,
                workspace_id,
                project_id,
                project_path,
                &command.command,
                &receipt_key,
                &recipe.recipe_hash,
                "approved",
                &options.trigger,
                &command.cwd,
                db_path,
                &options.generated_at,
            )?;
            receipt_ids.push(receipt_id.clone());
            let latest = store.setup_receipts(workspace_id)?;
            if latest
                .iter()
                .find(|receipt| receipt.id == receipt_id)
                .is_some_and(|receipt| receipt.state == "failed")
            {
                return Ok(PrewarmOutcome {
                    workspace_id: workspace_id.clone(),
                    project_id: project_id.clone(),
                    project_path: project_path.to_string(),
                    state: PrewarmState::SetupBlocked,
                    receipt_ids,
                    redacted_summary: "Setup stopped after the first failed command.".to_string(),
                });
            }
        }

        return Ok(PrewarmOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: PrewarmState::Hot,
            receipt_ids,
            redacted_summary: "Setup completed with redacted output.".to_string(),
        });
    }

    let Some(plan) = infer_setup_plan(project_root)? else {
        return Ok(PrewarmOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: PrewarmState::NoSetupNeeded,
            receipt_ids: Vec::new(),
            redacted_summary: "No setup recipe or safe lockfile restore was needed.".to_string(),
        });
    };

    if plan
        .commands
        .iter()
        .any(|command| command.approval_required)
        && !options.approve_setup
        && !inferred_commands_completed(store, workspace_id, project_id, &plan.commands)?
    {
        let receipt_id = setup_receipt_id(workspace_id, project_id, "inferred", "approval");
        let reasons = plan
            .commands
            .iter()
            .flat_map(|command| command.approval_reasons.clone())
            .collect::<Vec<_>>()
            .join("; ");
        store.upsert_setup_receipt(&SetupReceiptRecord {
            id: receipt_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            command: "inferred setup approval required".to_string(),
            state: "approval-required".to_string(),
            recipe_hash: "inferred".to_string(),
            approval_state: "required".to_string(),
            trigger: options.trigger.clone(),
            cwd: project_path.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            env_profile: "default".to_string(),
            output_path: None,
            redacted_summary: format!("Inferred setup needs approval: {reasons}"),
            receipt_json: serde_json::to_string(&ApprovalReceipt {
                recipe_hash: "inferred",
                source: "lockfiles",
                trigger: &options.trigger,
            })?,
            updated_at: options.generated_at.clone(),
        })?;
        append_setup_event(
            store,
            EventName::SetupBlocked,
            EventSeverity::Attention,
            "Inferred setup needs local approval before execution.",
            workspace_id,
            project_id,
            project_path,
            &receipt_id,
            &options.trigger,
            &options.generated_at,
        )?;
        return Ok(PrewarmOutcome {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            project_path: project_path.to_string(),
            state: PrewarmState::SetupBlocked,
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
        record_setup_approval(
            store,
            workspace_id,
            project_id,
            project_path,
            "inferred",
            "lockfiles",
            &options.trigger,
            &options.generated_at,
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

fn run_inferred_plan(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    commands: Vec<SetupCommandPlan>,
    db_path: &Path,
    options: &PrewarmOptions,
) -> Result<PrewarmOutcome, SetupRunError> {
    let mut receipt_ids = Vec::new();
    for command in commands {
        let command_text = command.command.join(" ");
        let recipe_hash = format!("inferred:{}", command.lockfile);
        let receipt_key = inferred_receipt_key(&command, &command_text)?;
        let expected_receipt_id =
            setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key);
        if setup_receipt_state(store, workspace_id, &expected_receipt_id)?
            .is_some_and(|state| state == "completed")
        {
            receipt_ids.push(expected_receipt_id);
            continue;
        }
        let receipt_id = run_shell_command(
            store,
            workspace_id,
            project_id,
            project_path,
            &command_text,
            &receipt_key,
            &recipe_hash,
            if command.approval_required {
                "approved"
            } else {
                "not-required"
            },
            &options.trigger,
            &command.cwd,
            db_path,
            &options.generated_at,
        )?;
        receipt_ids.push(receipt_id.clone());
        if store
            .setup_receipts(workspace_id)?
            .iter()
            .find(|receipt| receipt.id == receipt_id)
            .is_some_and(|receipt| receipt.state == "failed")
        {
            return Ok(PrewarmOutcome {
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: project_path.to_string(),
                state: PrewarmState::SetupBlocked,
                receipt_ids,
                redacted_summary: "Setup stopped after the first failed command.".to_string(),
            });
        }
    }
    Ok(PrewarmOutcome {
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        project_path: project_path.to_string(),
        state: PrewarmState::Hot,
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
        let recipe_hash = format!("inferred:{}", command.lockfile);
        let receipt_key = inferred_receipt_key(command, &command_text)?;
        let receipt_id = setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key);
        if setup_receipt_state(store, workspace_id, &receipt_id)?
            .is_none_or(|state| state != "completed")
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn run_shell_command(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    command_text: &str,
    receipt_key: &str,
    recipe_hash: &str,
    approval_state: &str,
    trigger: &str,
    cwd: &Path,
    db_path: &Path,
    now: &str,
) -> Result<String, SetupRunError> {
    let env_scope_root = store
        .current_workspace_root()?
        .ok_or(SetupRunError::MissingRoot)
        .map(PathBuf::from)?;
    let known_env_values = known_env_values(store, workspace_id, &env_scope_root, cwd)?;
    let command_redacted = redact_setup_text_with_values(command_text, &known_env_values);
    let redacted_command_text = command_redacted.text.clone();
    let receipt_id = setup_receipt_id(workspace_id, project_id, recipe_hash, receipt_key);
    append_setup_event(
        store,
        EventName::SetupStarted,
        EventSeverity::Info,
        "Setup command started; command text is redacted.",
        workspace_id,
        project_id,
        project_path,
        &receipt_id,
        trigger,
        now,
    )?;
    let output = run_bounded_shell_command(command_text, cwd)?;
    let (redacted_output, redaction_rules) = if output.output_limit_exceeded {
        (
            "[redacted] setup output exceeded the local log limit and was discarded.\n".to_string(),
            vec!["output-limit".to_string()],
        )
    } else {
        let combined = combined_output(&output.stdout, &output.stderr);
        let output_redacted = redact_setup_text_with_values(&combined, &known_env_values);
        (
            bounded_output_text(&output_redacted.text),
            output_redacted.rules,
        )
    };
    let command_completed = output.status.success() && !output.output_limit_exceeded;
    let state = if command_completed {
        "completed"
    } else {
        "failed"
    };
    let output_path = write_setup_log(db_path, &receipt_id, &redacted_output)?;
    let identity =
        collect_receipt_identity_inputs(cwd, "default", Some(recipe_hash.to_string()), None)?;

    store.upsert_setup_receipt(&SetupReceiptRecord {
        id: receipt_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: Some(project_id.clone()),
        command: redacted_command_text.clone(),
        state: state.to_string(),
        recipe_hash: recipe_hash.to_string(),
        approval_state: approval_state.to_string(),
        trigger: trigger.to_string(),
        cwd: project_path.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: "default".to_string(),
        output_path: Some(output_path.display().to_string()),
        redacted_summary: if output.output_limit_exceeded {
            "Setup command exceeded the output limit; output was discarded.".to_string()
        } else if command_completed {
            "Setup command completed; output is redacted.".to_string()
        } else {
            format!(
                "Setup command failed with status {:?}; output is redacted.",
                output.status.code()
            )
        },
        receipt_json: serde_json::to_string(&CommandReceipt {
            command: redacted_command_text,
            identity,
            redaction_rules,
        })?,
        updated_at: now.to_string(),
    })?;
    append_setup_event(
        store,
        if command_completed {
            EventName::SetupCompleted
        } else {
            EventName::SetupBlocked
        },
        if command_completed {
            EventSeverity::Info
        } else {
            EventSeverity::Attention
        },
        if command_completed {
            "Setup command completed; output is redacted."
        } else if output.output_limit_exceeded {
            "Setup command exceeded the output limit; output was discarded."
        } else {
            "Setup command failed; output is redacted."
        },
        workspace_id,
        project_id,
        project_path,
        &receipt_id,
        trigger,
        now,
    )?;
    Ok(receipt_id)
}

fn setup_recipe_metadata(path: &Path) -> Result<Option<fs::Metadata>, SetupRunError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SetupRunError::Io(error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_setup_approval(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    recipe_hash: &str,
    source: &str,
    trigger: &str,
    now: &str,
) -> Result<String, SetupRunError> {
    let receipt_id = setup_receipt_id(workspace_id, project_id, recipe_hash, "approval");
    store.upsert_setup_receipt(&SetupReceiptRecord {
        id: receipt_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: Some(project_id.clone()),
        command: "setup approval granted".to_string(),
        state: "approved".to_string(),
        recipe_hash: recipe_hash.to_string(),
        approval_state: "approved".to_string(),
        trigger: trigger.to_string(),
        cwd: project_path.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: "default".to_string(),
        output_path: None,
        redacted_summary: "Setup approval granted locally before execution.".to_string(),
        receipt_json: serde_json::to_string(&ApprovalReceipt {
            recipe_hash,
            source,
            trigger,
        })?,
        updated_at: now.to_string(),
    })?;
    Ok(receipt_id)
}

#[allow(clippy::too_many_arguments)]
fn append_setup_event(
    store: &MetadataStore,
    name: EventName,
    severity: EventSeverity,
    summary: &str,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    receipt_id: &str,
    trigger: &str,
    now: &str,
) -> Result<(), SetupRunError> {
    let mut event = WorkspaceEvent::new(
        EventId::new(setup_event_id(name, receipt_id, now)),
        name,
        now,
        severity,
        summary,
        workspace_id.clone(),
    );
    event.project_id = Some(project_id.clone());
    event.path = Some(project_path.to_string());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::SetupReceipt,
        id: receipt_id.to_string(),
        path: Some(project_path.to_string()),
    });
    event
        .payload
        .insert("trigger".to_string(), trigger.to_string().into());
    match store.append_event(event) {
        Ok(_) | Err(LocalEventError::DuplicateEventId(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn setup_event_id(name: EventName, receipt_id: &str, now: &str) -> String {
    let input = format!("{name:?}:{receipt_id}:{now}");
    format!("evt_setup_{}", blake3::hash(input.as_bytes()).to_hex())
}

fn shell_command(command_text: &str) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(command_text);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command.arg("-lc").arg(command_text);
        command
    }
}

fn run_bounded_shell_command(command_text: &str, cwd: &Path) -> io::Result<CapturedCommandOutput> {
    let mut command = shell_command(command_text);
    command
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_setup_command(&mut command);
    let mut child = command.spawn()?;
    let child_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("setup command stdout pipe was not available"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("setup command stderr pipe was not available"))?;
    let retained = Arc::new(AtomicUsize::new(0));
    let output_limit_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_reader = spawn_bounded_reader(
        stdout,
        Arc::clone(&retained),
        Arc::clone(&output_limit_exceeded),
    );
    let stderr_reader = spawn_bounded_reader(
        stderr,
        Arc::clone(&retained),
        Arc::clone(&output_limit_exceeded),
    );

    let mut killed_for_output_limit = false;
    let status = loop {
        if output_limit_exceeded.load(Ordering::Relaxed) && !killed_for_output_limit {
            kill_setup_process_tree(&mut child, child_id);
            killed_for_output_limit = true;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    terminate_setup_process_group(child_id);

    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    Ok(CapturedCommandOutput {
        stdout,
        stderr,
        status,
        output_limit_exceeded: output_limit_exceeded.load(Ordering::Relaxed),
    })
}

#[cfg(unix)]
fn configure_setup_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_setup_command(_command: &mut Command) {}

fn kill_setup_process_tree(child: &mut Child, child_id: u32) {
    terminate_setup_process_group(child_id);
    let _ = child.kill();
}

#[cfg(unix)]
fn terminate_setup_process_group(child_id: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{child_id}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn terminate_setup_process_group(_child_id: u32) {}

fn spawn_bounded_reader<R>(
    reader: R,
    retained: Arc<AtomicUsize>,
    output_limit_exceeded: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || read_bounded_pipe(reader, retained, output_limit_exceeded))
}

fn read_bounded_pipe<R>(
    mut reader: R,
    retained: Arc<AtomicUsize>,
    output_limit_exceeded: Arc<AtomicBool>,
) -> io::Result<Vec<u8>>
where
    R: Read,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(output);
        }
        let previous = retained.fetch_add(read, Ordering::Relaxed);
        if previous < MAX_CAPTURED_OUTPUT {
            let remaining = MAX_CAPTURED_OUTPUT - previous;
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if previous + read > MAX_CAPTURED_OUTPUT {
            output_limit_exceeded.store(true, Ordering::Relaxed);
        }
    }
}

fn join_reader(handle: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| io::Error::other("setup output reader panicked"))?
}

struct CapturedCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
    output_limit_exceeded: bool,
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

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(stdout.len() + stderr.len() + 1);
    bytes.extend_from_slice(stdout);
    if !stderr.is_empty() {
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(stderr);
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn bounded_output_text(text: &str) -> String {
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

fn inferred_receipt_key(
    command: &SetupCommandPlan,
    command_text: &str,
) -> Result<String, SetupRunError> {
    let recipe_hash = format!("inferred:{}", command.lockfile);
    let identity = collect_receipt_identity_inputs(
        &command.cwd,
        "default",
        Some(recipe_hash),
        Some(command.package_manager.clone()),
    )?;
    let identity_json = serde_json::to_string(&identity)?;
    let identity_hash = blake3::hash(identity_json.as_bytes());
    Ok(format!(
        "lockfile:{}:identity:{}:{}",
        command.lockfile,
        identity_hash.to_hex(),
        command_text
    ))
}

fn recipe_receipt_key(
    command: &super::SetupRecipeCommand,
    recipe_hash: &str,
) -> Result<String, SetupRunError> {
    let identity = collect_receipt_identity_inputs(
        &command.cwd,
        "default",
        Some(recipe_hash.to_string()),
        None,
    )?;
    let identity_json = serde_json::to_string(&identity)?;
    let identity_hash = blake3::hash(identity_json.as_bytes());
    Ok(format!(
        "line:{}:{}:identity:{}",
        command.line_number,
        command.command,
        identity_hash.to_hex()
    ))
}

fn known_env_values(
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
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env")
}

fn write_setup_log(db_path: &Path, receipt_id: &str, text: &str) -> io::Result<PathBuf> {
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
    Ok(store.setup_receipts(workspace_id)?.iter().any(|receipt| {
        receipt.project_id.as_ref() == Some(project_id)
            && receipt.recipe_hash == recipe_hash
            && matches!(receipt.state.as_str(), "completed" | "approved")
    }))
}

fn setup_receipt_state(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    receipt_id: &str,
) -> Result<Option<String>, SetupRunError> {
    Ok(store
        .setup_receipts(workspace_id)?
        .into_iter()
        .find(|receipt| receipt.id == receipt_id)
        .map(|receipt| receipt.state))
}

fn setup_receipt_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    recipe_hash: &str,
    command: &str,
) -> String {
    let input = format!(
        "{}:{}:{}:{}",
        workspace_id.as_str(),
        project_id.as_str(),
        recipe_hash,
        command
    );
    format!("setup_{}", blake3::hash(input.as_bytes()).to_hex())
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

impl fmt::Display for SetupRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "setup run failed: {error}"),
            Self::Recipe(error) => error.fmt(formatter),
            Self::Inference(error) => error.fmt(formatter),
            Self::UnsafeWorkspacePath(path) => {
                write!(
                    formatter,
                    "setup path {path} is not a normal directory below the accepted workspace"
                )
            }
            Self::MissingWorkspace => formatter.write_str("bowline workspace is not initialized"),
            Self::MissingRoot => formatter.write_str("bowline workspace root is not initialized"),
            Self::MissingProject(path) => {
                write!(formatter, "no bowline project found for {path}")
            }
            Self::Json(error) => write!(formatter, "setup receipt JSON failed: {error}"),
            Self::Events(error) => error.fmt(formatter),
        }
    }
}

impl Error for SetupRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Recipe(error) => Some(error),
            Self::Inference(error) => Some(error),
            Self::UnsafeWorkspacePath(_) => None,
            Self::Json(error) => Some(error),
            Self::Events(error) => Some(error),
            Self::MissingWorkspace | Self::MissingRoot | Self::MissingProject(_) => None,
        }
    }
}

impl From<MetadataError> for SetupRunError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for SetupRunError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<super::SetupRecipeError> for SetupRunError {
    fn from(error: super::SetupRecipeError) -> Self {
        Self::Recipe(error)
    }
}

impl From<SetupInferenceError> for SetupRunError {
    fn from(error: SetupInferenceError) -> Self {
        Self::Inference(error)
    }
}

impl From<serde_json::Error> for SetupRunError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<LocalEventError> for SetupRunError {
    fn from(error: LocalEventError) -> Self {
        Self::Events(error)
    }
}

#[cfg(test)]
mod tests {
    use super::write_setup_log;
    use crate::workspace::TempWorkspace;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn setup_log_writer_replaces_stale_symlink_without_following_it() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};

        let state = TempWorkspace::new("setup-log-state").expect("state");
        let outside = TempWorkspace::new("setup-log-outside").expect("outside");
        let db_path = state.root().join("metadata.sqlite3");
        let log_dir = state.root().join("setup-logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        let outside_target = outside.root().join("target");
        fs::write(&outside_target, b"outside").expect("outside");
        symlink(&outside_target, log_dir.join("setup_test.log")).expect("log symlink");

        let log_path =
            write_setup_log(&db_path, "setup_test", "SECRET=[redacted]").expect("log write");

        assert_eq!(
            fs::read(outside_target).expect("outside unchanged"),
            b"outside"
        );
        assert_eq!(
            fs::read_to_string(&log_path).expect("log text"),
            "SECRET=[redacted]"
        );
        assert_eq!(
            fs::metadata(log_path)
                .expect("log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
