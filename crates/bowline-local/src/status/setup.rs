use super::*;

pub(super) fn project_setup_readiness(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    projects: &[ProjectRecord],
    project_id: Option<&ProjectId>,
    workspace_root: Option<&str>,
) -> Result<Option<ProjectSetupReadinessReport>, LocalStatusError> {
    let (Some(project_id), Some(workspace_root)) = (project_id, workspace_root) else {
        return Ok(None);
    };
    let Some(project) = projects.iter().find(|project| project.id == *project_id) else {
        return Ok(Some(ProjectSetupReadinessReport::without_actions(
            readiness_unknown("Project metadata is missing; setup readiness cannot be determined."),
        )));
    };
    let project_root = PathBuf::from(workspace_root).join(&project.path);
    match std::fs::symlink_metadata(&project_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    "Project directory is a symlink and setup readiness cannot be determined safely.",
                    Some(
                        "Replace the project path with a normal directory inside the workspace."
                            .to_string(),
                    ),
                    None,
                ),
            )));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    "Project path is not a normal directory; setup readiness cannot be determined.",
                    Some(
                        "Replace the project path with a normal directory inside the workspace."
                            .to_string(),
                    ),
                    None,
                ),
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_unknown(
                    "Project directory is not materialized locally; setup readiness cannot be determined.",
                ),
            )));
        }
        Err(error) => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    &format!("Project directory metadata could not be read: {error}."),
                    Some("Fix local file permissions, then rerun status.".to_string()),
                    None,
                ),
            )));
        }
    }
    let recipe_path = project_root.join(".bowlinesetup");
    match std::fs::symlink_metadata(&recipe_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    "Setup recipe is a symlink and will not be executed.",
                    Some(
                        "Replace `.bowlinesetup` with a normal file inside the project."
                            .to_string(),
                    ),
                    None,
                ),
            )));
        }
        Ok(metadata) if metadata.is_file() => {
            return recipe_setup_readiness(store, workspace_id, project_id, &project_root);
        }
        Ok(_) => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    "Setup recipe path is not a normal file.",
                    Some("Replace `.bowlinesetup` with a normal file or remove it.".to_string()),
                    None,
                ),
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    &format!("Setup recipe metadata could not be read: {error}."),
                    Some("Fix local file permissions, then rerun status.".to_string()),
                    None,
                ),
            )));
        }
    }

    match crate::setup::infer_setup_plan(&project_root) {
        Ok(Some(plan)) => inferred_setup_readiness(store, workspace_id, project_id, plan),
        Ok(None) => Ok(Some(ProjectSetupReadinessReport::without_actions(
            ProjectSetupReadiness {
                state: ProjectSetupReadinessState::Runnable,
                reason: "No setup recipe or lockfile-backed restore is required.".to_string(),
                remedy: None,
                identity_hash: None,
                latest_receipt_id: None,
                latest_receipt_state: None,
                updated_at: None,
            },
        ))),
        Err(error) => Ok(Some(ProjectSetupReadinessReport::without_actions(
            readiness_blocked(
                &format!("Setup inference failed: {error}."),
                Some("Fix the unreadable setup metadata, then rerun status.".to_string()),
                None,
            ),
        ))),
    }
}

pub(super) fn apply_project_setup_readiness(
    report: Option<&ProjectSetupReadinessReport>,
    project_id: Option<&ProjectId>,
    acc: &mut StatusAccumulator,
) {
    let Some(report) = report else {
        return;
    };
    let readiness = &report.readiness;
    if readiness.state == ProjectSetupReadinessState::Runnable {
        return;
    }

    let kind = match readiness.state {
        ProjectSetupReadinessState::Unknown => "setup.readiness_unknown",
        ProjectSetupReadinessState::NeedsSetup => "setup.required",
        ProjectSetupReadinessState::Blocked => "setup.blocked",
        ProjectSetupReadinessState::Runnable => return,
    };
    acc.observe_fact(
        kind,
        format!(
            "setup:{}",
            project_id.map_or("workspace", ProjectId::as_str)
        ),
        format!(
            "setup:{}",
            project_id.map_or("workspace", ProjectId::as_str)
        ),
        StatusFactScope::Project,
        project_id.map(ProjectId::as_str),
    );
    let summary = format!(
        "Project setup readiness is {}: {}",
        readiness.state.as_str(),
        readiness.reason
    );
    acc.attention_items.push(summary.clone());
    let mut item = base_status_item(StatusItemKind::Setup, &summary);
    if let Some(project_id) = project_id {
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Project,
            id: project_id.as_str().to_string(),
            path: None,
        });
        item.project_id = Some(project_id.clone());
    }
    acc.items.push(item);
    for action in &report.actions {
        if !acc
            .next_actions
            .iter()
            .any(|existing| existing.command == action.command)
        {
            acc.next_actions.push(action.clone());
        }
    }
}

#[derive(Clone)]
pub(super) struct ProjectSetupReadinessReport {
    pub(super) readiness: ProjectSetupReadiness,
    pub(super) actions: Vec<RepairCommand>,
}

impl ProjectSetupReadinessReport {
    fn without_actions(readiness: ProjectSetupReadiness) -> Self {
        Self {
            readiness,
            actions: Vec::new(),
        }
    }

    fn with_actions(readiness: ProjectSetupReadiness, actions: Vec<RepairCommand>) -> Self {
        Self { readiness, actions }
    }
}

fn recipe_setup_readiness(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_root: &Path,
) -> Result<Option<ProjectSetupReadinessReport>, LocalStatusError> {
    let recipe =
        match crate::setup::load_setup_recipe(project_root, project_root.join(".bowlinesetup")) {
            Ok(recipe) => recipe,
            Err(error) => {
                return Ok(Some(ProjectSetupReadinessReport::without_actions(
                    readiness_blocked(
                        &format!("Setup recipe could not be loaded: {error}."),
                        Some(
                            "Fix `.bowlinesetup`, then rerun setup for the hot project."
                                .to_string(),
                        ),
                        None,
                    ),
                )));
            }
        };
    let identity = match crate::setup::collect_setup_identity(
        project_root,
        "default",
        Some(recipe.recipe_hash.clone()),
        None,
    ) {
        Ok(identity) => identity,
        Err(error) => {
            return Ok(Some(ProjectSetupReadinessReport::without_actions(
                readiness_blocked(
                    &format!("Setup identity could not be computed: {error}."),
                    Some("Fix unreadable setup identity files, then rerun status.".to_string()),
                    None,
                ),
            )));
        }
    };
    if recipe.commands.is_empty() {
        return Ok(Some(ProjectSetupReadinessReport::without_actions(
            ProjectSetupReadiness {
                state: ProjectSetupReadinessState::Runnable,
                reason: "Setup recipe contains no runnable commands.".to_string(),
                remedy: None,
                identity_hash: Some(identity.hash),
                latest_receipt_id: None,
                latest_receipt_state: None,
                updated_at: None,
            },
        )));
    }
    let mut latest_receipt = None;
    for command in &recipe.commands {
        let receipt_id =
            match recipe_receipt_id(workspace_id, project_id, command, &recipe.recipe_hash) {
                Ok(receipt_id) => receipt_id,
                Err(error) => {
                    return Ok(Some(ProjectSetupReadinessReport::without_actions(
                        readiness_blocked(
                            &format!("Setup receipt identity could not be computed: {error}."),
                            Some(
                                "Fix unreadable setup identity files, then rerun status."
                                    .to_string(),
                            ),
                            Some(identity.hash),
                        ),
                    )));
                }
            };
        let Some(receipt) = store.setup_receipt_by_id(workspace_id, &receipt_id)? else {
            return Ok(Some(ProjectSetupReadinessReport::with_actions(
                ProjectSetupReadiness {
                    state: ProjectSetupReadinessState::NeedsSetup,
                    reason: format!(
                        "Setup recipe command on line {} has not completed for the current setup identity.",
                        command.line_number
                    ),
                    remedy: Some(
                        "Approve setup locally, then rerun setup for the hot project.".to_string(),
                    ),
                    identity_hash: Some(identity.hash),
                    latest_receipt_id: None,
                    latest_receipt_state: None,
                    updated_at: None,
                },
                vec![setup_action(project_root, true)],
            )));
        };
        let readiness = readiness_from_receipt(
            receipt.clone(),
            Some(identity.hash.clone()),
            setup_action(project_root, true),
        );
        if readiness.readiness.state != ProjectSetupReadinessState::Runnable {
            return Ok(Some(readiness));
        }
        latest_receipt = Some(receipt);
    }
    let latest_receipt = latest_receipt.expect("non-empty setup recipe has latest receipt");
    Ok(Some(ProjectSetupReadinessReport::without_actions(
        ProjectSetupReadiness {
            state: ProjectSetupReadinessState::Runnable,
            reason: format!(
                "{} setup recipe command receipt(s) match the current setup identity.",
                recipe.commands.len()
            ),
            remedy: None,
            identity_hash: Some(identity.hash),
            latest_receipt_state: latest_receipt_state(&latest_receipt),
            latest_receipt_id: Some(latest_receipt.id),
            updated_at: Some(latest_receipt.updated_at),
        },
    )))
}

fn recipe_receipt_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    command: &crate::setup::SetupRecipeCommand,
    recipe_hash: &str,
) -> std::io::Result<String> {
    let receipt_key = crate::setup::recipe_receipt_key(command, recipe_hash)?;
    Ok(crate::setup::setup_receipt_id(
        workspace_id,
        project_id,
        recipe_hash,
        &receipt_key,
    ))
}

fn inferred_setup_readiness(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    plan: crate::setup::SetupPlan,
) -> Result<Option<ProjectSetupReadinessReport>, LocalStatusError> {
    let mut receipt_count = 0usize;
    let mut first_identity_hash = None;
    for command in &plan.commands {
        let identity = match crate::setup::collect_setup_identity(
            &command.cwd,
            "default",
            Some(crate::setup::inferred_recipe_hash(command)),
            Some(command.package_manager.clone()),
        ) {
            Ok(identity) => identity,
            Err(error) => {
                return Ok(Some(ProjectSetupReadinessReport::without_actions(
                    readiness_blocked(
                        &format!("Setup identity could not be computed: {error}."),
                        Some("Fix unreadable setup identity files, then rerun status.".to_string()),
                        None,
                    ),
                )));
            }
        };
        first_identity_hash.get_or_insert_with(|| identity.hash.clone());
        let command_text = command.command.join(" ");
        let receipt_key = match crate::setup::inferred_receipt_key(command, &command_text) {
            Ok(receipt_key) => receipt_key,
            Err(error) => {
                return Ok(Some(ProjectSetupReadinessReport::without_actions(
                    readiness_blocked(
                        &format!("Setup receipt identity could not be computed: {error}."),
                        Some("Fix unreadable setup identity files, then rerun status.".to_string()),
                        Some(identity.hash),
                    ),
                )));
            }
        };
        let recipe_hash = crate::setup::inferred_recipe_hash(command);
        let receipt_id =
            crate::setup::setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key);
        let Some(receipt) = store.setup_receipt_by_id(workspace_id, &receipt_id)? else {
            let reason = if command.approval_required {
                format!(
                    "Inferred setup needs approval: {}",
                    command.approval_reasons.join("; ")
                )
            } else {
                format!(
                    "Lockfile-backed setup has not run for {}.",
                    command.lockfile
                )
            };
            return Ok(Some(ProjectSetupReadinessReport::with_actions(
                ProjectSetupReadiness {
                    state: ProjectSetupReadinessState::NeedsSetup,
                    reason,
                    remedy: Some(
                        "Run setup for this hot project on the current machine.".to_string(),
                    ),
                    identity_hash: Some(identity.hash),
                    latest_receipt_id: None,
                    latest_receipt_state: None,
                    updated_at: None,
                },
                vec![setup_action(&command.cwd, command.approval_required)],
            )));
        };
        let readiness = readiness_from_receipt(
            receipt.clone(),
            Some(identity.hash.clone()),
            setup_action(&command.cwd, command.approval_required),
        );
        if readiness.readiness.state != ProjectSetupReadinessState::Runnable {
            return Ok(Some(readiness));
        }
        if let Some(output_path) = missing_inferred_setup_output(command) {
            return Ok(Some(ProjectSetupReadinessReport::with_actions(
                ProjectSetupReadiness {
                    state: ProjectSetupReadinessState::NeedsSetup,
                    reason: format!(
                        "Setup output `{}` is missing for {}.",
                        output_path.display(),
                        command.lockfile
                    ),
                    remedy: Some(
                        "Run setup for this hot project on the current machine.".to_string(),
                    ),
                    identity_hash: Some(identity.hash),
                    latest_receipt_state: latest_receipt_state(&receipt),
                    latest_receipt_id: Some(receipt.id),
                    updated_at: Some(receipt.updated_at),
                },
                vec![setup_action(&command.cwd, command.approval_required)],
            )));
        }
        receipt_count += 1;
    }
    Ok(Some(ProjectSetupReadinessReport::without_actions(
        ProjectSetupReadiness {
            state: ProjectSetupReadinessState::Runnable,
            reason: format!(
                "{receipt_count} lockfile-backed setup receipt(s) match the current setup identity."
            ),
            remedy: None,
            identity_hash: first_identity_hash,
            latest_receipt_id: None,
            latest_receipt_state: None,
            updated_at: None,
        },
    )))
}

fn missing_inferred_setup_output(command: &crate::setup::SetupCommandPlan) -> Option<PathBuf> {
    let expected = match command.manager.as_str() {
        "pnpm" | "npm" | "bun" => command.cwd.join("node_modules"),
        "uv" => command.cwd.join(".venv"),
        _ => return None,
    };
    if expected.is_dir() {
        None
    } else {
        Some(expected)
    }
}

fn readiness_from_receipt(
    receipt: crate::metadata::SetupReceiptRecord,
    identity_hash: Option<String>,
    setup_action: RepairCommand,
) -> ProjectSetupReadinessReport {
    let state = readiness_state_from_receipt(&receipt);
    let actions = if state == ProjectSetupReadinessState::NeedsSetup {
        vec![setup_action]
    } else {
        Vec::new()
    };
    ProjectSetupReadinessReport::with_actions(
        ProjectSetupReadiness {
            state,
            reason: if receipt.readiness_reason.is_empty() {
                receipt.redacted_summary.clone()
            } else {
                receipt.readiness_reason.clone()
            },
            remedy: if receipt.readiness_remedy.is_empty() {
                None
            } else {
                Some(receipt.readiness_remedy.clone())
            },
            identity_hash,
            latest_receipt_state: latest_receipt_state(&receipt),
            latest_receipt_id: Some(receipt.id),
            updated_at: Some(receipt.updated_at),
        },
        actions,
    )
}

fn readiness_state_from_receipt(
    receipt: &crate::metadata::SetupReceiptRecord,
) -> ProjectSetupReadinessState {
    match ProjectSetupReadinessState::from_wire(&receipt.readiness_state) {
        Some(
            state @ (ProjectSetupReadinessState::Runnable
            | ProjectSetupReadinessState::NeedsSetup
            | ProjectSetupReadinessState::Blocked),
        ) => state,
        Some(ProjectSetupReadinessState::Unknown) | None => receipt_lifecycle_readiness(receipt),
    }
}

fn receipt_lifecycle_readiness(
    receipt: &crate::metadata::SetupReceiptRecord,
) -> ProjectSetupReadinessState {
    match SetupReceiptState::from_wire(&receipt.state) {
        Some(SetupReceiptState::Completed) => ProjectSetupReadinessState::Runnable,
        Some(SetupReceiptState::Approved | SetupReceiptState::ApprovalRequired) => {
            ProjectSetupReadinessState::NeedsSetup
        }
        Some(SetupReceiptState::Failed) => ProjectSetupReadinessState::Blocked,
        None if receipt.state == "blocked" => ProjectSetupReadinessState::Blocked,
        None => ProjectSetupReadinessState::Unknown,
    }
}

fn latest_receipt_state(
    receipt: &crate::metadata::SetupReceiptRecord,
) -> Option<SetupReceiptState> {
    SetupReceiptState::from_wire(&receipt.state)
}

fn readiness_unknown(reason: &str) -> ProjectSetupReadiness {
    ProjectSetupReadiness {
        state: ProjectSetupReadinessState::Unknown,
        reason: reason.to_string(),
        remedy: None,
        identity_hash: None,
        latest_receipt_id: None,
        latest_receipt_state: None,
        updated_at: None,
    }
}

fn readiness_blocked(
    reason: &str,
    remedy: Option<String>,
    identity_hash: Option<String>,
) -> ProjectSetupReadiness {
    ProjectSetupReadiness {
        state: ProjectSetupReadinessState::Blocked,
        reason: reason.to_string(),
        remedy,
        identity_hash,
        latest_receipt_id: None,
        latest_receipt_state: None,
        updated_at: None,
    }
}

fn setup_action(project_root: &Path, approve: bool) -> RepairCommand {
    let approve_flag = if approve { " --yes" } else { "" };
    RepairCommand::mutating(
        if approve {
            "Approve and run setup".to_string()
        } else {
            "Run setup".to_string()
        },
        Some(format!(
            "bowline setup {}{}",
            shell_word(&project_root.display().to_string()),
            approve_flag
        )),
    )
}
