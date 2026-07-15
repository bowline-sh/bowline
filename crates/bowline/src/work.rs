use std::path::PathBuf;

use bowline_core::commands::{
    CommandName, WorkCleanupCommandOutput, WorkCreateCommandOutput, WorkDiffCommandOutput,
    WorkLifecycleCommandOutput, WorkListCommandOutput,
};
use bowline_core::ids::DeviceId;
use bowline_local::work_views::{
    WorkCleanupOptions, WorkCreateOptions, WorkListOptions, WorkSelectorOptions,
    WorkViewAcceptPhase, WorkViewAcceptProgress, WorkViewError, cleanup_work_views,
    create_work_view, diff_work_view, discard_work_view, enqueue_work_view_accept, list_work_views,
    restore_work_view, work_view_accept_progress,
};

use crate::surface::style::{self, Presentation, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCreateArgs {
    pub project_path: String,
    pub name: String,
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkListArgs {
    pub include_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSelectorArgs {
    pub selector: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCleanupArgs {
    pub apply: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkLifecycle {
    Accept,
    Discard,
    Restore,
}

impl WorkLifecycle {
    pub fn command_name(self) -> CommandName {
        match self {
            Self::Accept => CommandName::Accept,
            Self::Discard => CommandName::Discard,
            Self::Restore => CommandName::Restore,
        }
    }
}

pub fn run_work_create(
    args: WorkCreateArgs,
    db_path: Option<PathBuf>,
    owner_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkCreateCommandOutput, WorkViewError> {
    create_work_view(WorkCreateOptions {
        db_path,
        project_path: args.project_path,
        name: args.name,
        base_snapshot_selector: args.from,
        owner_device_id: Some(owner_device_id),
        generated_at,
    })
}

pub fn run_list(
    args: WorkListArgs,
    db_path: Option<PathBuf>,
    current_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkListCommandOutput, WorkViewError> {
    list_work_views(WorkListOptions {
        db_path,
        include_hidden: args.include_hidden,
        current_device_id: Some(current_device_id),
        generated_at,
    })
}

pub fn run_diff(
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkDiffCommandOutput, WorkViewError> {
    diff_work_view(WorkSelectorOptions {
        db_path,
        selector: args.selector,
        paths: args.paths,
        generated_at,
    })
}

pub fn run_lifecycle(
    lifecycle: WorkLifecycle,
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    current_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    run_lifecycle_with_progress(
        lifecycle,
        args,
        db_path,
        current_device_id,
        generated_at,
        |_| {},
    )
}

pub fn run_lifecycle_with_progress(
    lifecycle: WorkLifecycle,
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    current_device_id: DeviceId,
    generated_at: String,
    mut on_progress: impl FnMut(&WorkViewAcceptProgress),
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let options = WorkSelectorOptions {
        db_path,
        selector: args.selector,
        paths: args.paths,
        generated_at,
    };
    match lifecycle {
        WorkLifecycle::Accept => run_durable_accept(options, current_device_id, &mut on_progress),
        WorkLifecycle::Discard => discard_work_view(options),
        WorkLifecycle::Restore => restore_work_view(options),
    }
}

fn run_durable_accept(
    options: WorkSelectorOptions,
    current_device_id: DeviceId,
    on_progress: &mut impl FnMut(&WorkViewAcceptProgress),
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let db_path = options.db_path.clone();
    let generated_at = options.generated_at.clone();
    let operation = enqueue_work_view_accept(options, current_device_id)?;
    crate::wire::wake_durable_work_best_effort();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let progress =
            work_view_accept_progress(db_path.as_deref(), &operation.id, generated_at.clone())?;
        on_progress(&progress);
        match progress {
            WorkViewAcceptProgress::Terminal(output) => return Ok(*output),
            WorkViewAcceptProgress::Pending { state, .. } => {
                if std::time::Instant::now() >= deadline {
                    return Err(WorkViewError::AcceptOperationPending {
                        operation_id: operation.id,
                        state,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

pub fn render_accept_progress_human(progress: &WorkViewAcceptProgress) -> Option<String> {
    let WorkViewAcceptProgress::Pending {
        phase,
        completed_steps,
        total_steps,
        partial,
        ..
    } = progress
    else {
        return None;
    };
    let scope = if *partial { "partial" } else { "full" };
    Some(format!(
        "Accept ({scope})  {}  {completed_steps}/{total_steps}\n",
        accept_phase_label(*phase)
    ))
}

fn accept_phase_label(phase: WorkViewAcceptPhase) -> &'static str {
    match phase {
        WorkViewAcceptPhase::Queued => "queued",
        WorkViewAcceptPhase::CandidateBuilt => "candidate built",
        WorkViewAcceptPhase::MainFenceRechecked => "main fence rechecked",
        WorkViewAcceptPhase::ObjectsUploaded => "objects uploaded",
        WorkViewAcceptPhase::SnapshotStaged => "snapshot staged",
        WorkViewAcceptPhase::MainPublished => "main published",
        WorkViewAcceptPhase::WorkspaceRefPublished => "workspace ref published",
        WorkViewAcceptPhase::LifecyclePublished => "lifecycle published",
        WorkViewAcceptPhase::WaitingRetry => "waiting to retry",
    }
}

pub fn run_cleanup(
    args: WorkCleanupArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkCleanupCommandOutput, WorkViewError> {
    cleanup_work_views(WorkCleanupOptions {
        db_path,
        apply: args.apply,
        generated_at,
    })
}

pub fn render_work_create_human(output: &WorkCreateCommandOutput) -> String {
    let pres = Presentation::detect(false);
    format!(
        "{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("Path", &pres),
        output.work_view.visible_path,
        style::section("State", &pres),
        style::paint("active", Role::Ready, &pres),
    )
}

pub fn render_list_human(output: &WorkListCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work views", &pres),
        output.work_views.len()
    )];
    lines.extend(output.work_views.iter().map(|view| {
        format!(
            "  {}  {}  {}",
            style::paint(&view.name, Role::Strong, &pres),
            style::paint(&view.visible_path, Role::Label, &pres),
            style::paint(&style::kebab(&view.lifecycle), Role::Label, &pres),
        )
    }));
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_diff_human(output: &WorkDiffCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres)
    )];
    if output.changes.is_empty() {
        lines.push(format!(
            "  {}",
            style::paint("No local changes recorded.", Role::Label, &pres)
        ));
    } else {
        lines.extend(output.changes.iter().map(|change| {
            let redacted = if change.contains_secrets {
                style::paint("  (redacted)", Role::Label, &pres)
            } else {
                String::new()
            };
            format!(
                "  {} {}{redacted}",
                style::paint(&style::kebab(&change.kind), Role::Label, &pres),
                change.path,
            )
        }));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_lifecycle_human(output: &WorkLifecycleCommandOutput) -> String {
    let pres = Presentation::detect(false);
    format!(
        "{}  {}\n{}  {}\n\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("State", &pres),
        style::kebab(&output.work_view.lifecycle),
    )
}

pub fn render_cleanup_human(output: &WorkCleanupCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Cleanup candidates", &pres),
        output.previewed_paths.len()
    )];
    if output.deleted_paths.is_empty() {
        lines.extend(
            output
                .previewed_paths
                .iter()
                .map(|path| format!("  {}", style::paint(path, Role::Label, &pres))),
        );
    } else {
        lines.extend(
            output
                .deleted_paths
                .iter()
                .map(|path| format!("  {} {path}", style::paint("deleted", Role::Limited, &pres))),
        );
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use bowline_local::metadata::WorkViewAcceptOperationState;

    use super::*;

    #[test]
    fn accept_progress_renders_exact_full_and_partial_lifecycle() {
        let full = WorkViewAcceptProgress::Pending {
            operation_id: "accept_full".to_string(),
            state: WorkViewAcceptOperationState::Claimed,
            phase: WorkViewAcceptPhase::ObjectsUploaded,
            completed_steps: 3,
            total_steps: 7,
            partial: false,
        };
        let partial = WorkViewAcceptProgress::Pending {
            operation_id: "accept_partial".to_string(),
            state: WorkViewAcceptOperationState::WaitingRetry,
            phase: WorkViewAcceptPhase::WaitingRetry,
            completed_steps: 4,
            total_steps: 7,
            partial: true,
        };

        assert_eq!(
            render_accept_progress_human(&full).as_deref(),
            Some("Accept (full)  objects uploaded  3/7\n")
        );
        assert_eq!(
            render_accept_progress_human(&partial).as_deref(),
            Some("Accept (partial)  waiting to retry  4/7\n")
        );
    }
}
