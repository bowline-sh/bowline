use std::path::PathBuf;

use bowline_core::commands::{
    CommandName, WorkCleanupCommandOutput, WorkDiffCommandOutput, WorkLifecycleCommandOutput,
    WorkListCommandOutput, WorkonCommandOutput,
};
use bowline_core::ids::DeviceId;
use bowline_local::work_views::{
    WorkCleanupOptions, WorkListOptions, WorkSelectorOptions, WorkViewError, WorkonOptions,
    accept_work_view, cleanup_work_views, create_work_view, diff_work_view, discard_work_view,
    list_work_views, restore_work_view,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkonArgs {
    pub project_path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkListArgs {
    pub include_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSelectorArgs {
    pub selector: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCleanupArgs {
    pub apply: bool,
}

pub fn run_workon(
    args: WorkonArgs,
    db_path: Option<PathBuf>,
    owner_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkonCommandOutput, WorkViewError> {
    create_work_view(WorkonOptions {
        db_path,
        project_path: args.project_path,
        name: args.name,
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
        generated_at,
    })
}

pub fn run_lifecycle(
    command: CommandName,
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let options = WorkSelectorOptions {
        db_path,
        selector: args.selector,
        generated_at,
    };
    match command {
        CommandName::Accept => accept_work_view(options),
        CommandName::Discard => discard_work_view(options),
        CommandName::Restore => restore_work_view(options),
        _ => unreachable!("unsupported work lifecycle command"),
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

pub fn render_workon_human(output: &WorkonCommandOutput) -> String {
    format!(
        "Work view: {}\nPath: {}\nState: active\n\n",
        output.work_view.name, output.work_view.visible_path
    )
}

pub fn render_list_human(output: &WorkListCommandOutput) -> String {
    let mut lines = vec![format!("Work views: {}", output.work_views.len())];
    lines.extend(output.work_views.iter().map(|view| {
        format!(
            "  {}  {}  {}",
            view.name,
            view.visible_path,
            serde_json::to_value(view.lifecycle)
                .expect("lifecycle serializes")
                .as_str()
                .unwrap_or("unknown")
        )
    }));
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_diff_human(output: &WorkDiffCommandOutput) -> String {
    let mut lines = vec![format!("Work view: {}", output.work_view.name)];
    if output.changes.is_empty() {
        lines.push("No local changes recorded.".to_string());
    } else {
        lines.extend(output.changes.iter().map(|change| {
            format!(
                "  {:?} {}{}",
                change.kind,
                change.path,
                if change.contains_secrets {
                    " (redacted)"
                } else {
                    ""
                }
            )
        }));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_lifecycle_human(output: &WorkLifecycleCommandOutput) -> String {
    format!(
        "Work view: {}\nState: {}\n\n",
        output.work_view.name,
        serde_json::to_value(output.work_view.lifecycle)
            .expect("lifecycle serializes")
            .as_str()
            .unwrap_or("unknown")
    )
}

pub fn render_cleanup_human(output: &WorkCleanupCommandOutput) -> String {
    let mut lines = vec![format!(
        "Cleanup candidates: {}",
        output.previewed_paths.len()
    )];
    if output.deleted_paths.is_empty() {
        lines.extend(
            output
                .previewed_paths
                .iter()
                .map(|path| format!("  {path}")),
        );
    } else {
        lines.extend(
            output
                .deleted_paths
                .iter()
                .map(|path| format!("  deleted {path}")),
        );
    }
    lines.push(String::new());
    lines.join("\n")
}
