use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    commands::{
        CONTRACT_VERSION, CommandName, NamespaceLifecycleAction, NamespaceLifecycleCommandOutput,
        NamespaceLifecyclePreview,
    },
    events::{
        EventActor, EventActorKind, EventName, EventSeverity, EventSubject, EventSubjectKind,
        WorkspaceEvent,
    },
    ids::{EventId, ProjectId, WorkspaceId},
    shell::quote_word,
    status::RepairCommand,
};
use serde_json::Value;

use crate::metadata::{
    MetadataError, MetadataStore, ProjectLifecycleState, ProjectLocalMaterializationState,
    ProjectRecord, default_database_path,
};

const DEFAULT_PURGE_GRACE_DAYS: u32 = 14;
const MIN_PURGE_GRACE_DAYS: u32 = 1;
const MAX_PURGE_GRACE_DAYS: u32 = 90;
/// Above this many entries the preview stops enumerating individual files and
/// reports one row per top-level directory instead. A delete preview that
/// enumerates a `node_modules` tree into JSON is unreadable and unbounded.
const MAX_PREVIEW_PATHS: usize = 1000;

/// Whether the caller has proof that this project's local work is already
/// pushed. `forget_local` destroys an entire project tree including untracked
/// files and `.env`, so the daemon's answer is a precondition, not advice —
/// "no answer" is refused exactly like "not converged".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncConvergence {
    Converged,
    PendingWork { paths: Vec<String> },
    Unproven(ConvergenceUnproven),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergenceUnproven {
    DaemonUnreachable,
    DaemonNotReady,
    ProbeFailed,
}

impl ConvergenceUnproven {
    fn as_str(self) -> &'static str {
        match self {
            Self::DaemonUnreachable => "the sync daemon is not reachable",
            Self::DaemonNotReady => "the sync daemon has not reached a ready state",
            Self::ProbeFailed => "the sync state probe failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamespaceLifecycleOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub generated_at: String,
    pub yes: bool,
    pub dry_run: bool,
    pub restore: bool,
    pub cancel: bool,
    pub grace_days: Option<u32>,
    pub convergence: SyncConvergence,
}

#[derive(Debug)]
pub enum NamespaceLifecycleError {
    Metadata(MetadataError),
    Io(io::Error),
    ProjectMissing(String),
    ConfirmationRequired,
    UnsyncedWork { paths: Vec<String> },
    ConvergenceUnproven(ConvergenceUnproven),
    InvalidState(String),
    EventAppend(String),
}

pub fn forget_local(
    options: NamespaceLifecycleOptions,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let context = LifecycleContext::open(&options.project_path, options.db_path)?;
    let preview = deletion_preview(&context.project_path)?;
    if options.dry_run {
        return Ok(output(
            CommandName::ForgetLocal,
            NamespaceLifecycleAction::ForgetLocal,
            &options.generated_at,
            &context,
            preview,
            false,
            vec![RepairCommand::mutating(
                "Delete these local bytes on this device".to_string(),
                Some(format!(
                    "bowline forget-local {} --yes",
                    quote_word(&context.project.path)
                )),
            )],
        ));
    }
    // Confirmation is local and answerable offline, so it gates first: a caller
    // who omitted --yes must be told that, not handed a daemon probe failure
    // they cannot act on.
    if !options.yes {
        return Err(NamespaceLifecycleError::ConfirmationRequired);
    }
    ensure_no_unsynced_work(&options.convergence)?;
    if context.project_path.exists() {
        fs::remove_dir_all(&context.project_path)?;
    }
    context.store.set_project_local_materialization(
        &context.workspace_id,
        &context.project.id,
        ProjectLocalMaterializationState::Forgotten,
    )?;
    append_lifecycle_event(
        &context.store,
        EventName::NamespaceDeletedOrArchived,
        &context.workspace_id,
        &context.project,
        &options.generated_at,
        "Project local bytes forgotten on this device.",
        "forget-local",
    )?;
    Ok(output(
        CommandName::ForgetLocal,
        NamespaceLifecycleAction::ForgetLocal,
        &options.generated_at,
        &context,
        preview,
        true,
        vec![RepairCommand::inspect(
            "Re-materialize with setup".to_string(),
            Some(format!(
                "bowline setup {}",
                quote_word(&context.project.path)
            )),
        )],
    ))
}

pub fn archive(
    options: NamespaceLifecycleOptions,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let context = LifecycleContext::open(&options.project_path, options.db_path)?;
    if options.dry_run {
        return Ok(output(
            CommandName::Archive,
            if options.restore {
                NamespaceLifecycleAction::Restore
            } else {
                NamespaceLifecycleAction::Archive
            },
            &options.generated_at,
            &context,
            deletion_preview(&context.project_path)?,
            false,
            Vec::new(),
        ));
    }
    ensure_no_unsynced_work(&options.convergence)?;
    if options.restore {
        context.store.set_project_lifecycle(
            &context.workspace_id,
            &context.project.id,
            ProjectLifecycleState::Active,
            None,
            None,
        )?;
        append_lifecycle_event(
            &context.store,
            EventName::NamespaceDeletedOrArchived,
            &context.workspace_id,
            &context.project,
            &options.generated_at,
            "Project archive restored.",
            "archive-restore",
        )?;
        return Ok(output(
            CommandName::Archive,
            NamespaceLifecycleAction::Restore,
            &options.generated_at,
            &context,
            deletion_preview(&context.project_path)?,
            true,
            Vec::new(),
        ));
    }
    context.store.set_project_lifecycle(
        &context.workspace_id,
        &context.project.id,
        ProjectLifecycleState::Archived,
        None,
        None,
    )?;
    append_lifecycle_event(
        &context.store,
        EventName::NamespaceDeletedOrArchived,
        &context.workspace_id,
        &context.project,
        &options.generated_at,
        "Project archived; local bytes retained.",
        "archive",
    )?;
    Ok(output(
        CommandName::Archive,
        NamespaceLifecycleAction::Archive,
        &options.generated_at,
        &context,
        deletion_preview(&context.project_path)?,
        true,
        vec![RepairCommand::mutating(
            "Forget the local copy on this device".to_string(),
            Some(format!(
                "bowline forget-local {} --dry-run",
                quote_word(&context.project.path)
            )),
        )],
    ))
}

pub fn purge(
    options: NamespaceLifecycleOptions,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let context = LifecycleContext::open(&options.project_path, options.db_path)?;
    if context.project.lifecycle_state != ProjectLifecycleState::Archived
        && !options.cancel
        && context.project.lifecycle_state != ProjectLifecycleState::PurgePending
    {
        return Err(NamespaceLifecycleError::InvalidState(
            "purge requires an archived project".to_string(),
        ));
    }
    let mut preview = deletion_preview(&context.project_path)?;
    preview.pack_count = pack_count(&context.store, &context.workspace_id, &context.project.id)?;

    if options.dry_run {
        preview.grace_days = Some(clamp_grace_days(options.grace_days)?);
        return Ok(output(
            CommandName::Purge,
            if options.cancel {
                NamespaceLifecycleAction::PurgeCancel
            } else {
                NamespaceLifecycleAction::PurgePending
            },
            &options.generated_at,
            &context,
            preview,
            false,
            Vec::new(),
        ));
    }

    if options.cancel {
        if context.project.lifecycle_state != ProjectLifecycleState::PurgePending {
            return Err(NamespaceLifecycleError::InvalidState(
                "purge cancel requires a purge-pending project".to_string(),
            ));
        }
        context.store.set_project_lifecycle(
            &context.workspace_id,
            &context.project.id,
            ProjectLifecycleState::Archived,
            None,
            None,
        )?;
        append_lifecycle_event(
            &context.store,
            EventName::NamespaceDeletedOrArchived,
            &context.workspace_id,
            &context.project,
            &options.generated_at,
            "Project purge cancelled; archive retained.",
            "purge-cancel",
        )?;
        return Ok(output(
            CommandName::Purge,
            NamespaceLifecycleAction::PurgeCancel,
            &options.generated_at,
            &context,
            preview,
            true,
            Vec::new(),
        ));
    }

    let grace_days = clamp_grace_days(options.grace_days)?;
    preview.grace_days = Some(grace_days);
    // Fourteen days gives normal device/offline recovery a real chance to
    // notice before remote ciphertext becomes GC-eligible.
    let purge_after = purge_after_iso(&options.generated_at, grace_days)?;
    preview.purge_after = Some(purge_after.clone());
    context.store.set_project_lifecycle(
        &context.workspace_id,
        &context.project.id,
        ProjectLifecycleState::PurgePending,
        None,
        Some(&purge_after),
    )?;
    append_lifecycle_event(
        &context.store,
        EventName::NamespaceDeletedOrArchived,
        &context.workspace_id,
        &context.project,
        &options.generated_at,
        "Project purge scheduled; local bytes retained.",
        "purge-pending",
    )?;
    Ok(output(
        CommandName::Purge,
        NamespaceLifecycleAction::PurgePending,
        &options.generated_at,
        &context,
        preview,
        true,
        vec![RepairCommand::mutating(
            "Cancel purge during the grace window".to_string(),
            Some(format!(
                "bowline purge {} --cancel",
                quote_word(&context.project.path)
            )),
        )],
    ))
}

struct LifecycleContext {
    store: MetadataStore,
    workspace_id: WorkspaceId,
    project: ProjectRecord,
    project_path: PathBuf,
}

impl LifecycleContext {
    fn open(
        requested_project: &str,
        db_path: Option<PathBuf>,
    ) -> Result<Self, NamespaceLifecycleError> {
        let db_path = match db_path {
            Some(path) => path,
            None => default_database_path()?,
        };
        let store = MetadataStore::open(&db_path)?;
        let workspace = store.current_workspace()?.ok_or_else(|| {
            NamespaceLifecycleError::ProjectMissing(requested_project.to_string())
        })?;
        let project = store
            .project_by_path(&workspace.id, requested_project)?
            .ok_or_else(|| {
                NamespaceLifecycleError::ProjectMissing(requested_project.to_string())
            })?;
        let workspace_root = store.workspace_root(&workspace.id)?.ok_or_else(|| {
            NamespaceLifecycleError::ProjectMissing(requested_project.to_string())
        })?;
        let project_path = Path::new(&workspace_root).join(&project.path);
        Ok(Self {
            store,
            workspace_id: workspace.id,
            project,
            project_path,
        })
    }
}

fn ensure_no_unsynced_work(convergence: &SyncConvergence) -> Result<(), NamespaceLifecycleError> {
    match convergence {
        SyncConvergence::Converged => Ok(()),
        SyncConvergence::PendingWork { paths } => Err(NamespaceLifecycleError::UnsyncedWork {
            paths: paths.clone(),
        }),
        SyncConvergence::Unproven(reason) => {
            Err(NamespaceLifecycleError::ConvergenceUnproven(*reason))
        }
    }
}

fn deletion_preview(path: &Path) -> Result<NamespaceLifecyclePreview, NamespaceLifecycleError> {
    let mut walk = PreviewWalk::default();
    if path.exists() {
        collect_preview(path, path, &mut walk)?;
    }
    let mut paths = if walk.paths.len() > MAX_PREVIEW_PATHS {
        walk.top_level
    } else {
        walk.paths
    };
    paths.sort();
    Ok(NamespaceLifecyclePreview {
        paths,
        byte_total: walk.byte_total,
        pack_count: 0,
        grace_days: None,
        purge_after: None,
    })
}

#[derive(Default)]
struct PreviewWalk {
    paths: Vec<String>,
    top_level: Vec<String>,
    byte_total: u64,
}

fn collect_preview(
    root: &Path,
    path: &Path,
    walk: &mut PreviewWalk,
) -> Result<(), NamespaceLifecycleError> {
    let metadata = fs::symlink_metadata(path)?;
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string();
    if relative.is_empty() {
        walk.paths.push(".".to_string());
    } else {
        if !relative.contains('/') {
            walk.top_level.push(relative.clone());
        }
        walk.paths.push(relative);
    }
    if metadata.is_file() {
        walk.byte_total = walk.byte_total.saturating_add(metadata.len());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_preview(root, &entry?.path(), walk)?;
        }
    }
    Ok(())
}

fn pack_count(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<u64, NamespaceLifecycleError> {
    Ok(store
        .project_latest_snapshot_id(workspace_id, project_id)?
        .map(|_| 1)
        .unwrap_or(0))
}

fn append_lifecycle_event(
    store: &MetadataStore,
    name: EventName,
    workspace_id: &WorkspaceId,
    project: &ProjectRecord,
    generated_at: &str,
    summary: &str,
    action: &str,
) -> Result<(), NamespaceLifecycleError> {
    let mut event = WorkspaceEvent::new(
        lifecycle_event_id(action, project.id.as_str(), generated_at),
        name,
        generated_at,
        EventSeverity::Attention,
        summary,
        workspace_id.clone(),
    );
    event.project_id = Some(project.id.clone());
    event.path = Some(project.path.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Project,
        id: project.id.as_str().to_string(),
        path: Some(project.path.clone()),
    });
    event.actor = Some(EventActor {
        kind: EventActorKind::User,
        id: None,
        display_name: None,
    });
    event
        .payload
        .insert("action".to_string(), Value::String(action.to_string()));
    store
        .append_event(event)
        .map_err(|error| NamespaceLifecycleError::EventAppend(error.to_string()))?;
    Ok(())
}

fn lifecycle_event_id(action: &str, subject: &str, generated_at: &str) -> EventId {
    let input = format!("{action}:{subject}:{generated_at}");
    EventId::new(format!(
        "evt_lifecycle_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

fn output(
    command: CommandName,
    action: NamespaceLifecycleAction,
    generated_at: &str,
    context: &LifecycleContext,
    preview: NamespaceLifecyclePreview,
    changed: bool,
    next_actions: Vec<RepairCommand>,
) -> NamespaceLifecycleCommandOutput {
    NamespaceLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at.to_string(),
        workspace_id: context.workspace_id.clone(),
        project_id: context.project.id.clone(),
        project_path: context.project.path.clone(),
        action,
        preview,
        changed,
        next_actions,
    }
}

fn clamp_grace_days(value: Option<u32>) -> Result<u32, NamespaceLifecycleError> {
    let days = value.unwrap_or(DEFAULT_PURGE_GRACE_DAYS);
    if !(MIN_PURGE_GRACE_DAYS..=MAX_PURGE_GRACE_DAYS).contains(&days) {
        return Err(NamespaceLifecycleError::InvalidState(format!(
            "purge grace must be between {MIN_PURGE_GRACE_DAYS} and {MAX_PURGE_GRACE_DAYS} days"
        )));
    }
    Ok(days)
}

fn purge_after_iso(now: &str, grace_days: u32) -> Result<String, NamespaceLifecycleError> {
    let parsed = time::OffsetDateTime::parse(now, &time::format_description::well_known::Rfc3339)
        .map_err(|error| NamespaceLifecycleError::InvalidState(error.to_string()))?;
    (parsed + time::Duration::days(i64::from(grace_days)))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| NamespaceLifecycleError::InvalidState(error.to_string()))
}

impl fmt::Display for NamespaceLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "lifecycle file operation failed: {error}"),
            Self::ProjectMissing(path) => write!(formatter, "project `{path}` was not found"),
            Self::ConfirmationRequired => write!(
                formatter,
                "this lifecycle command needs --yes after previewing the changes"
            ),
            Self::UnsyncedWork { paths } => {
                write!(
                    formatter,
                    "project has unsynced local work: {}",
                    paths.join(", ")
                )
            }
            Self::ConvergenceUnproven(reason) => {
                write!(
                    formatter,
                    "refusing to delete local bytes: {}",
                    reason.as_str()
                )
            }
            Self::InvalidState(message) => formatter.write_str(message),
            Self::EventAppend(message) => {
                write!(formatter, "lifecycle audit append failed: {message}")
            }
        }
    }
}

impl Error for NamespaceLifecycleError {}

impl From<MetadataError> for NamespaceLifecycleError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for NamespaceLifecycleError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{metadata::ProjectLocalMaterializationState, workspace::TempWorkspace};

    const NOW: &str = "2026-07-09T12:00:00Z";

    #[test]
    fn forget_local_removes_local_bytes_and_appends_lifecycle_event() {
        let fixture = fixture("lifecycle-forget-clean");

        let output = forget_local(options(&fixture, "apps/web")).unwrap();

        assert_eq!(output.command, CommandName::ForgetLocal);
        assert!(output.changed);
        assert_eq!(output.preview.byte_total, 6);
        assert!(!fixture.temp.root().join("apps/web").exists());
        let project = fixture
            .store
            .project_by_id(&fixture.workspace_id, &fixture.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            project.local_materialization_state,
            ProjectLocalMaterializationState::Forgotten
        );
        let events = fixture.store.list_events(10).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.name == EventName::NamespaceDeletedOrArchived)
        );
    }

    #[test]
    fn archive_restore_and_purge_state_machine_are_typed() {
        let fixture = fixture("lifecycle-archive-purge");

        let archived = archive(options(&fixture, "apps/web")).unwrap();
        assert_eq!(archived.command, CommandName::Archive);
        let project = fixture
            .store
            .project_by_id(&fixture.workspace_id, &fixture.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(project.lifecycle_state, ProjectLifecycleState::Archived);
        assert!(fixture.temp.root().join("apps/web/src/index.ts").exists());

        let purge_output = purge(NamespaceLifecycleOptions {
            grace_days: Some(2),
            ..options(&fixture, "apps/web")
        })
        .unwrap();
        assert_eq!(purge_output.command, CommandName::Purge);
        assert_eq!(purge_output.preview.grace_days, Some(2));
        let project = fixture
            .store
            .project_by_id(&fixture.workspace_id, &fixture.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(project.lifecycle_state, ProjectLifecycleState::PurgePending);
        assert_eq!(project.purge_after.as_deref(), Some("2026-07-11T12:00:00Z"));

        let cancelled = purge(NamespaceLifecycleOptions {
            cancel: true,
            ..options(&fixture, "apps/web")
        })
        .unwrap();
        assert_eq!(cancelled.action, NamespaceLifecycleAction::PurgeCancel);
        let restored = archive(NamespaceLifecycleOptions {
            restore: true,
            ..options(&fixture, "apps/web")
        })
        .unwrap();
        assert_eq!(restored.action, NamespaceLifecycleAction::Restore);
        let project = fixture
            .store
            .project_by_id(&fixture.workspace_id, &fixture.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(project.lifecycle_state, ProjectLifecycleState::Active);
    }

    #[test]
    fn purge_requires_archived_project_and_bounds_grace() {
        let fixture = fixture("lifecycle-purge-bounds");

        let live_error = purge(options(&fixture, "apps/web")).expect_err("active project refused");
        assert!(matches!(
            live_error,
            NamespaceLifecycleError::InvalidState(_)
        ));

        archive(options(&fixture, "apps/web")).unwrap();
        let grace_error = purge(NamespaceLifecycleOptions {
            grace_days: Some(91),
            ..options(&fixture, "apps/web")
        })
        .expect_err("out of range grace refused");
        assert!(matches!(
            grace_error,
            NamespaceLifecycleError::InvalidState(_)
        ));
    }

    struct Fixture {
        temp: TempWorkspace,
        store: MetadataStore,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        db_path: PathBuf,
    }

    fn fixture(name: &str) -> Fixture {
        let temp = TempWorkspace::new(name).unwrap();
        temp.write_project_file("apps/web", "src/index.ts", b"hello\n")
            .unwrap();
        let db_path = temp.root().join("local.sqlite3");
        let store = MetadataStore::open(&db_path).unwrap();
        let workspace_id = WorkspaceId::new("ws_lifecycle");
        let project_id = ProjectId::new("proj_web");
        store.insert_workspace(&workspace_id, "Code", NOW).unwrap();
        store
            .insert_root("root", &workspace_id, &temp.root().to_string_lossy(), NOW)
            .unwrap();
        store
            .insert_project(&project_id, &workspace_id, "root", "apps/web", NOW)
            .unwrap();
        Fixture {
            temp,
            store,
            workspace_id,
            project_id,
            db_path,
        }
    }

    fn options(fixture: &Fixture, project_path: &str) -> NamespaceLifecycleOptions {
        NamespaceLifecycleOptions {
            db_path: Some(fixture.db_path.clone()),
            project_path: project_path.to_string(),
            generated_at: NOW.to_string(),
            yes: true,
            dry_run: false,
            restore: false,
            cancel: false,
            grace_days: None,
            convergence: SyncConvergence::Converged,
        }
    }

    #[test]
    fn forget_local_refuses_to_delete_unsynced_work() {
        let fixture = fixture("lifecycle-forget-unsynced");

        let error = forget_local(NamespaceLifecycleOptions {
            convergence: SyncConvergence::PendingWork {
                paths: vec!["apps/web/.env".to_string()],
            },
            ..options(&fixture, "apps/web")
        })
        .expect_err("pending work blocks deletion");

        assert!(matches!(
            error,
            NamespaceLifecycleError::UnsyncedWork { ref paths }
                if paths == &vec!["apps/web/.env".to_string()]
        ));
        assert!(fixture.temp.root().join("apps/web/src/index.ts").exists());
    }

    #[test]
    fn forget_local_refuses_when_sync_state_cannot_be_proven() {
        let fixture = fixture("lifecycle-forget-unproven");

        let error = forget_local(NamespaceLifecycleOptions {
            convergence: SyncConvergence::Unproven(ConvergenceUnproven::DaemonUnreachable),
            ..options(&fixture, "apps/web")
        })
        .expect_err("an unreachable daemon blocks deletion");

        assert!(matches!(
            error,
            NamespaceLifecycleError::ConvergenceUnproven(ConvergenceUnproven::DaemonUnreachable)
        ));
        assert!(fixture.temp.root().join("apps/web/src/index.ts").exists());
    }

    #[test]
    fn forget_local_dry_run_returns_the_real_preview_without_deleting() {
        let fixture = fixture("lifecycle-forget-dry-run");

        let output = forget_local(NamespaceLifecycleOptions {
            dry_run: true,
            yes: false,
            convergence: SyncConvergence::Unproven(ConvergenceUnproven::DaemonUnreachable),
            ..options(&fixture, "apps/web")
        })
        .expect("dry run succeeds without confirmation or a daemon");

        assert!(!output.changed);
        assert_eq!(output.preview.byte_total, 6);
        assert!(
            output
                .preview
                .paths
                .iter()
                .any(|path| path == "src/index.ts")
        );
        assert!(fixture.temp.root().join("apps/web/src/index.ts").exists());
    }

    #[test]
    fn deletion_preview_summarises_top_level_directories_for_huge_trees() {
        let fixture = fixture("lifecycle-preview-cap");
        for index in 0..(MAX_PREVIEW_PATHS + 8) {
            fixture
                .temp
                .write_project_file("apps/web", format!("node_modules/pkg{index}.js"), b"x")
                .unwrap();
        }

        let preview = deletion_preview(fixture.temp.root().join("apps/web").as_path()).unwrap();

        assert!(preview.paths.len() < MAX_PREVIEW_PATHS);
        assert!(preview.paths.iter().any(|path| path == "node_modules"));
        assert!(preview.paths.iter().any(|path| path == "src"));
        assert!(!preview.paths.iter().any(|path| path.contains('/')));
    }
}
