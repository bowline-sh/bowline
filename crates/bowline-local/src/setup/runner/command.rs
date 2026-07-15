use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupApprovalState {
    Approved,
    Required,
    NotRequired,
}

impl SetupApprovalState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Required => "required",
            Self::NotRequired => "not-required",
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            "approved" => Some(Self::Approved),
            "required" => Some(Self::Required),
            "not-required" => Some(Self::NotRequired),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub(super) struct SetupCommandContext<'a> {
    pub(super) store: &'a MetadataStore,
    pub(super) workspace_id: &'a WorkspaceId,
    pub(super) project_id: &'a ProjectId,
    pub(super) project_path: &'a str,
    pub(super) project_root: &'a Path,
    pub(super) trigger: &'a str,
    pub(super) db_path: &'a Path,
    pub(super) now: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct SetupShellCommand<'a> {
    pub(super) command_text: &'a str,
    pub(super) receipt_key: &'a str,
    pub(super) recipe_hash: &'a str,
    pub(super) approval_state: SetupApprovalState,
    pub(super) package_manager: Option<&'a PackageManagerIdentity>,
    pub(super) cwd: &'a Path,
}

#[derive(Clone, Copy)]
pub(super) struct SetupApprovalReceipt<'a> {
    pub(super) recipe_hash: &'a str,
    pub(super) source: &'a str,
}

#[derive(Clone)]
pub(super) struct SetupEventRecord<'a> {
    pub(super) name: EventName,
    pub(super) severity: EventSeverity,
    pub(super) summary: &'a str,
    pub(super) receipt_id: &'a str,
}

pub(super) fn run_shell_command(
    context: SetupCommandContext<'_>,
    command: SetupShellCommand<'_>,
) -> Result<String, SetupRunError> {
    let store = context.store;
    let env_scope_root = store
        .current_workspace_root()?
        .ok_or(SetupRunError::MissingRoot)
        .map(PathBuf::from)?;
    let known_env_values =
        known_env_values(store, context.workspace_id, &env_scope_root, command.cwd)?;
    let command_redacted = redact_setup_text_with_values(command.command_text, &known_env_values);
    let redacted_command_text = command_redacted.text.clone();
    let receipt_id = setup_receipt_id(
        context.workspace_id,
        context.project_id,
        command.recipe_hash,
        command.receipt_key,
    );
    append_setup_event(
        &context,
        SetupEventRecord {
            name: EventName::SetupStarted,
            severity: EventSeverity::Info,
            summary: "Setup command started; command text is redacted.",
            receipt_id: &receipt_id,
        },
    )?;
    let output = run_bounded_shell_command(command.command_text, command.cwd)?;
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
        SetupReceiptState::Completed
    } else {
        SetupReceiptState::Failed
    };
    let output_path = write_setup_log(context.db_path, &receipt_id, &redacted_output)?;
    let identity = collect_setup_identity(
        command.cwd,
        "default",
        Some(command.recipe_hash.to_string()),
        command.package_manager.cloned(),
    )?;
    let readiness = if command_completed {
        SetupReadinessClassification {
            state: SetupReadinessState::Runnable,
            reason: "Setup command completed for the current setup identity.".to_string(),
            remedy: None,
        }
    } else {
        classify_setup_command_result(
            command.command_text,
            output.status.code(),
            &redacted_output,
            output.output_limit_exceeded,
        )
    };
    debug_assert_eq!(
        SetupApprovalState::from_str(command.approval_state.as_str()),
        Some(command.approval_state)
    );

    store.upsert_setup_receipt(&SetupReceiptRecord {
        id: receipt_id.clone(),
        workspace_id: context.workspace_id.clone(),
        project_id: Some(context.project_id.clone()),
        command: redacted_command_text.clone(),
        state: state.as_str().to_string(),
        recipe_hash: command.recipe_hash.to_string(),
        approval_state: command.approval_state.as_str().to_string(),
        trigger: context.trigger.to_string(),
        cwd: context.project_path.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: "default".to_string(),
        output_path: Some(output_path.display().to_string()),
        redacted_summary: if output.output_limit_exceeded {
            "Setup command exceeded the output limit; output was discarded.".to_string()
        } else if command_completed {
            "Setup command completed; output is redacted.".to_string()
        } else {
            readiness.reason.clone()
        },
        setup_identity_hash: identity.hash,
        readiness_state: readiness.state.as_str().to_string(),
        readiness_reason: readiness.reason,
        readiness_remedy: readiness.remedy.unwrap_or_default(),
        receipt_json: serde_json::to_string(&CommandReceipt {
            command: redacted_command_text,
            identity: identity.inputs,
            redaction_rules,
        })?,
        updated_at: context.now.to_string(),
    })?;
    append_setup_event(
        &context,
        SetupEventRecord {
            name: if command_completed {
                EventName::SetupCompleted
            } else {
                EventName::SetupBlocked
            },
            severity: if command_completed {
                EventSeverity::Info
            } else {
                EventSeverity::Attention
            },
            summary: if command_completed {
                "Setup command completed; output is redacted."
            } else if output.output_limit_exceeded {
                "Setup command exceeded the output limit; output was discarded."
            } else {
                "Setup command failed; output is redacted."
            },
            receipt_id: &receipt_id,
        },
    )?;
    Ok(receipt_id)
}

pub(super) fn setup_recipe_metadata(path: &Path) -> Result<Option<fs::Metadata>, SetupRunError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SetupRunError::Io(error)),
    }
}

pub(super) fn record_setup_approval(
    context: &SetupCommandContext<'_>,
    approval: SetupApprovalReceipt<'_>,
) -> Result<String, SetupRunError> {
    let receipt_id = setup_receipt_id(
        context.workspace_id,
        context.project_id,
        approval.recipe_hash,
        "approval",
    );
    debug_assert_eq!(
        SetupApprovalState::from_str(SetupApprovalState::Approved.as_str()),
        Some(SetupApprovalState::Approved)
    );
    context.store.upsert_setup_receipt(&SetupReceiptRecord {
        id: receipt_id.clone(),
        workspace_id: context.workspace_id.clone(),
        project_id: Some(context.project_id.clone()),
        command: "setup approval granted".to_string(),
        state: SetupReceiptState::Approved.as_str().to_string(),
        recipe_hash: approval.recipe_hash.to_string(),
        approval_state: SetupApprovalState::Approved.as_str().to_string(),
        trigger: context.trigger.to_string(),
        cwd: context.project_path.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: "default".to_string(),
        output_path: None,
        redacted_summary: "Setup approval granted locally before execution.".to_string(),
        setup_identity_hash: collect_setup_identity(
            context.project_root,
            "default",
            Some(approval.recipe_hash.to_string()),
            None,
        )
        .map(|identity| identity.hash)?,
        readiness_state: SetupReadinessState::NeedsSetup.as_str().to_string(),
        readiness_reason: "Setup approval was granted; setup commands still need to run."
            .to_string(),
        readiness_remedy: "Rerun setup for the hot project.".to_string(),
        receipt_json: serde_json::to_string(&ApprovalReceipt {
            recipe_hash: approval.recipe_hash,
            source: approval.source,
            trigger: context.trigger,
        })?,
        updated_at: context.now.to_string(),
    })?;
    Ok(receipt_id)
}

pub(super) fn append_setup_event(
    context: &SetupCommandContext<'_>,
    record: SetupEventRecord<'_>,
) -> Result<(), SetupRunError> {
    let mut event = WorkspaceEvent::new(
        EventId::new(setup_event_id(&record.name, record.receipt_id, context.now)),
        record.name,
        context.now,
        record.severity,
        record.summary,
        context.workspace_id.clone(),
    );
    event.project_id = Some(context.project_id.clone());
    event.path = Some(context.project_path.to_string());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::SetupReceipt,
        id: record.receipt_id.to_string(),
        path: Some(context.project_path.to_string()),
    });
    event
        .payload
        .insert("trigger".to_string(), context.trigger.to_string().into());
    match context.store.append_event(event) {
        Ok(_) | Err(LocalEventError::DuplicateEventId(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn setup_event_id(name: &EventName, receipt_id: &str, now: &str) -> String {
    let input = format!("{name:?}:{receipt_id}:{now}");
    format!("evt_setup_{}", blake3::hash(input.as_bytes()).to_hex())
}

pub(super) fn shell_command(command_text: &str) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(command_text);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command.arg("-c").arg(command_text);
        command
    }
}

pub(super) fn run_bounded_shell_command(
    command_text: &str,
    cwd: &Path,
) -> io::Result<CapturedCommandOutput> {
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
            if let Err(error) = kill_setup_process_tree(&mut child, child_id) {
                tolerate_cleanup_error_if_readers_closed(error, &stdout_reader, &stderr_reader)?;
            }
            killed_for_output_limit = true;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    if let Err(error) = terminate_setup_process_group(child_id) {
        tolerate_cleanup_error_if_readers_closed(error, &stdout_reader, &stderr_reader)?;
    }

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
pub(super) fn configure_setup_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
pub(super) fn configure_setup_command(_command: &mut Command) {}

#[cfg(unix)]
pub(super) fn kill_setup_process_tree(child: &mut Child, child_id: u32) -> io::Result<()> {
    let group_result = terminate_setup_process_group(child_id);
    let _ = child.kill();
    group_result
}

#[cfg(not(unix))]
pub(super) fn kill_setup_process_tree(child: &mut Child, _child_id: u32) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
pub(super) fn terminate_setup_process_group(child_id: u32) -> io::Result<()> {
    use rustix::{
        io::Errno,
        process::{Pid, Signal, kill_process_group},
    };

    let raw_pid = i32::try_from(child_id)
        .map_err(|_| io::Error::other("setup process id exceeded the platform range"))?;
    let pid =
        Pid::from_raw(raw_pid).ok_or_else(|| io::Error::other("setup process id was zero"))?;
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(io::Error::from_raw_os_error(error.raw_os_error())),
    }
}

#[cfg(not(unix))]
pub(super) fn terminate_setup_process_group(_child_id: u32) -> io::Result<()> {
    Ok(())
}

fn tolerate_cleanup_error_if_readers_closed(
    error: io::Error,
    stdout_reader: &thread::JoinHandle<io::Result<Vec<u8>>>,
    stderr_reader: &thread::JoinHandle<io::Result<Vec<u8>>>,
) -> io::Result<()> {
    for _ in 0..5 {
        if stdout_reader.is_finished() && stderr_reader.is_finished() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(error)
}

pub(super) fn spawn_bounded_reader<R>(
    reader: R,
    retained: Arc<AtomicUsize>,
    output_limit_exceeded: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || read_bounded_pipe(reader, retained, output_limit_exceeded))
}

pub(super) fn read_bounded_pipe<R>(
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

pub(super) fn join_reader(handle: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| io::Error::other("setup output reader panicked"))?
}

pub(crate) struct CapturedCommandOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) status: ExitStatus,
    pub(crate) output_limit_exceeded: bool,
}
