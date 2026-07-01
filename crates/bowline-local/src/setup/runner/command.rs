use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn run_shell_command(
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

pub(super) fn setup_recipe_metadata(path: &Path) -> Result<Option<fs::Metadata>, SetupRunError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SetupRunError::Io(error)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_setup_approval(
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
pub(super) fn append_setup_event(
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

pub(super) fn setup_event_id(name: EventName, receipt_id: &str, now: &str) -> String {
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
        command.arg("-lc").arg(command_text);
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
pub(super) fn configure_setup_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
pub(super) fn configure_setup_command(_command: &mut Command) {}

pub(super) fn kill_setup_process_tree(child: &mut Child, child_id: u32) {
    terminate_setup_process_group(child_id);
    let _ = child.kill();
}

#[cfg(unix)]
pub(super) fn terminate_setup_process_group(child_id: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{child_id}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
pub(super) fn terminate_setup_process_group(_child_id: u32) {}

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

pub(super) struct CapturedCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
    output_limit_exceeded: bool,
}
