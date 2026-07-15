use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    error::Error,
    fmt, fs,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
};

use bowline_core::{
    commands::{RootChoiceState, RootInitOutput},
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{EventId, ProjectId, WorkspaceId},
    status::RepairCommand,
};

use crate::{
    env::{EnvImportError, import_env_records_from_scan},
    events::LocalEventError,
    metadata::{
        MetadataError, MetadataStore, ObservedLocalPath, ProjectUpsert, default_database_path,
    },
    scanner::{ScanError, ScanReport, scan_workspace},
};

const WORKSPACE_ID: &str = "ws_code";
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub db_path: Option<PathBuf>,
    pub requested_root: Option<String>,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSelection {
    pub root: PathBuf,
    pub display_root: String,
    pub root_choice: RootChoiceState,
    pub created_root: bool,
}

#[derive(Debug)]
pub enum LocalInitError {
    Io(io::Error),
    Metadata(MetadataError),
    Events(LocalEventError),
    EnvImport(EnvImportError),
    Scan(ScanError),
    AmbiguousDefaultRoot(Vec<PathBuf>),
}

pub fn initialize_root(options: InitOptions) -> Result<RootInitOutput, LocalInitError> {
    initialize_root_with_workspace(options, selected_workspace_id())
}

pub fn initialize_root_with_workspace(
    options: InitOptions,
    workspace_id: WorkspaceId,
) -> Result<RootInitOutput, LocalInitError> {
    let selection = select_or_create_root(options.requested_root.as_deref())?;
    let root = selection.root;

    let report = scan_workspace(&root)?;
    let db_path = options
        .db_path
        .clone()
        .map(Ok)
        .unwrap_or_else(default_database_path)?;
    let mut store = MetadataStore::open(db_path)?;
    persist_scan(
        &mut store,
        &workspace_id,
        &root,
        &report,
        &options.generated_at,
    )?;
    append_observed_event(&store, &workspace_id, &root, &report, &options.generated_at)?;

    Ok(RootInitOutput {
        generated_at: options.generated_at,
        workspace_id,
        root: selection.display_root.clone(),
        root_choice: selection.root_choice,
        observed_only: true,
        changed_workspace_files: false,
        created_root: selection.created_root,
        scan_summary: report.summary,
        non_actions: vec![
            "Did not move, rewrite, or delete project files.".to_string(),
            "Did not run Git commands or contact remotes.".to_string(),
            "Did not upload bytes or start sync.".to_string(),
            "Did not print environment variable values or upload env bytes during init."
                .to_string(),
        ],
        next_actions: vec![RepairCommand::inspect(
            "Inspect observed workspace".to_string(),
            Some(format!(
                "bowline status --root {}",
                shell_word(&display_path(&root))
            )),
        )],
    })
}

pub fn select_or_create_root(
    requested_root: Option<&str>,
) -> Result<RootSelection, LocalInitError> {
    let root = choose_root(requested_root)?;
    let created_root = ensure_root_exists(&root)?;
    let root_choice = match (requested_root.is_some(), created_root) {
        (true, false) => RootChoiceState::ExplicitExisting,
        (true, true) => RootChoiceState::ExplicitCreated,
        (false, true) => RootChoiceState::DefaultSelected,
        (false, false) => RootChoiceState::DefaultSelected,
    };
    Ok(RootSelection {
        display_root: display_path(&root),
        root,
        root_choice,
        created_root,
    })
}

impl fmt::Display for LocalInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "init I/O failed: {error}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::Events(error) => error.fmt(formatter),
            Self::EnvImport(error) => error.fmt(formatter),
            Self::Scan(error) => error.fmt(formatter),
            Self::AmbiguousDefaultRoot(paths) => write!(
                formatter,
                "multiple existing code roots found: {}",
                paths
                    .iter()
                    .map(|path| display_path(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl Error for LocalInitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::Events(error) => Some(error),
            Self::EnvImport(error) => Some(error),
            Self::Scan(error) => Some(error),
            Self::AmbiguousDefaultRoot(_) => None,
        }
    }
}

impl From<io::Error> for LocalInitError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MetadataError> for LocalInitError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<LocalEventError> for LocalInitError {
    fn from(error: LocalEventError) -> Self {
        Self::Events(error)
    }
}

impl From<EnvImportError> for LocalInitError {
    fn from(error: EnvImportError) -> Self {
        Self::EnvImport(error)
    }
}

impl From<ScanError> for LocalInitError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

fn choose_root(requested_root: Option<&str>) -> Result<PathBuf, LocalInitError> {
    if let Some(root) = requested_root {
        return resolve_user_path(root).map_err(Into::into);
    }

    let home = home_dir()?;
    let code_root = home.join("Code");
    let candidates = ["Code", "Projects", "dev", "src"]
        .into_iter()
        .map(|name| home.join(name))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();

    match candidates.as_slice() {
        [] => Ok(code_root),
        [only] if only == &code_root => Ok(code_root),
        _ => Err(LocalInitError::AmbiguousDefaultRoot(candidates)),
    }
}

fn ensure_root_exists(path: &Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    fs::create_dir_all(path)?;
    Ok(true)
}

fn selected_workspace_id() -> WorkspaceId {
    std::env::var("BOWLINE_WORKSPACE_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .map(WorkspaceId::new)
        .unwrap_or_else(|| WorkspaceId::new(WORKSPACE_ID))
}

fn persist_scan(
    store: &mut MetadataStore,
    workspace_id: &WorkspaceId,
    root: &Path,
    report: &ScanReport,
    now: &str,
) -> Result<(), LocalInitError> {
    let report = workspace_scoped_scan_report(workspace_id, report);
    store.insert_workspace(workspace_id, "User Code", now)?;
    let root_path = root.display().to_string();
    let root_id = store
        .accepted_root_id_for_path(workspace_id, &root_path)?
        .unwrap_or_else(|| workspace_scoped_root_id(workspace_id));
    store.insert_root(&root_id, workspace_id, &root_path, now)?;
    let projects = report
        .projects
        .iter()
        .map(|project| ProjectUpsert {
            id: project.id.clone(),
            path: project.path.clone(),
            git_observer_state: project.observer_state,
        })
        .collect::<Vec<_>>();
    store.replace_projects(workspace_id, &root_id, &projects, now)?;
    let paths = report
        .paths
        .iter()
        .map(|path| ObservedLocalPath {
            project_id: path.project_id.clone(),
            path: path.path.clone(),
            classification: path.policy.classification,
            mode: path.policy.mode,
            access: path.policy.access.clone(),
        })
        .collect::<Vec<_>>();
    store.replace_observed_paths(workspace_id, &paths, now)?;
    store.set_observed_summary(workspace_id, &report.summary, now)?;
    let env_import = import_env_records_from_scan(store, workspace_id, root, &report, None, now)?;
    if env_import.imported_record_count > 0 {
        append_env_imported_event(store, workspace_id, &env_import, now)?;
    }
    Ok(())
}

fn workspace_scoped_scan_report(workspace_id: &WorkspaceId, report: &ScanReport) -> ScanReport {
    let mut scoped = report.clone();
    let project_ids = scoped
        .projects
        .iter_mut()
        .map(|project| {
            let original = project.id.clone();
            project.id = workspace_scoped_project_id(workspace_id, &original);
            (original, project.id.clone())
        })
        .collect::<BTreeMap<_, _>>();
    for path in &mut scoped.paths {
        if let Some(project_id) = &path.project_id
            && let Some(scoped_project_id) = project_ids.get(project_id)
        {
            path.project_id = Some(scoped_project_id.clone());
        }
    }
    scoped
}

fn workspace_scoped_project_id(workspace_id: &WorkspaceId, project_id: &ProjectId) -> ProjectId {
    ProjectId::new(format!(
        "proj_{}_{}",
        id_component(workspace_id.as_str()),
        id_component(project_id.as_str())
    ))
}

fn workspace_scoped_root_id(workspace_id: &WorkspaceId) -> String {
    format!("root_{}", id_component(workspace_id.as_str()))
}

fn id_component(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    output.trim_matches('_').to_string()
}

fn append_env_imported_event(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    report: &crate::env::EnvImportReport,
    now: &str,
) -> Result<(), LocalInitError> {
    let mut event = WorkspaceEvent::new(
        EventId::new(format!(
            "evt_env_imported_{}",
            blake3::hash(format!("{}:{now}", workspace_id.as_str()).as_bytes()).to_hex()
        )),
        EventName::EnvImported,
        now,
        EventSeverity::Info,
        format!(
            "Imported {} project env record(s) from {} file(s); values are redacted.",
            report.imported_record_count, report.imported_file_count
        ),
        workspace_id.clone(),
    );
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    event.payload.insert(
        "recordCount".to_string(),
        serde_json::Value::from(report.imported_record_count as u64),
    );
    event.payload.insert(
        "fileCount".to_string(),
        serde_json::Value::from(report.imported_file_count as u64),
    );

    match store.append_event(event) {
        Ok(_) | Err(LocalEventError::DuplicateEventId(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn append_observed_event(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    root: &Path,
    report: &ScanReport,
    now: &str,
) -> Result<(), LocalInitError> {
    let mut event = WorkspaceEvent::new(
        EventId::new(event_id(root, now)),
        EventName::PolicyClassified,
        now,
        EventSeverity::Info,
        "Workspace observed locally; sync has not started.",
        workspace_id.clone(),
    );
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: Some(display_path(root)),
    });
    event
        .payload
        .insert("repoCount".to_string(), report.summary.repo_count.into());
    event.payload.insert(
        "workspaceSyncPathCount".to_string(),
        report.summary.workspace_sync_path_count.into(),
    );

    match store.append_event(event) {
        Ok(_) | Err(LocalEventError::DuplicateEventId(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn event_id(root: &Path, now: &str) -> String {
    let mut hasher = DefaultHasher::new();
    root.display().to_string().hash(&mut hasher);
    now.hash(&mut hasher);
    format!("evt_init_observed_{:x}", hasher.finish())
}

fn resolve_user_path(path: &str) -> io::Result<PathBuf> {
    let expanded = if path == "~" || path.starts_with("~/") {
        let home = home_dir()?;
        if path == "~" {
            home
        } else {
            home.join(path.trim_start_matches("~/"))
        }
    } else {
        PathBuf::from(path)
    };

    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        std::env::current_dir().map(|cwd| cwd.join(expanded))
    }
}

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn display_path(path: &Path) -> String {
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

fn shell_word(value: &str) -> String {
    if value == "~" {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", bowline_core::shell::quote_word(rest));
    }
    bowline_core::shell::quote_word(value)
}

#[cfg(test)]
mod tests {
    use super::{InitOptions, initialize_root, initialize_root_with_workspace};
    use crate::metadata::MetadataStore;
    use bowline_core::{
        ids::WorkspaceId, policy::PathClassification, workspace_graph::WorkspaceRelativePath,
    };

    #[test]
    fn init_existing_root_observes_without_mutating_workspace_files() {
        let temp = crate::workspace::TempWorkspace::new("init-existing").expect("temp workspace");
        let code_root = temp.root().join("Code");
        std::fs::create_dir_all(&code_root).expect("code root");
        std::fs::create_dir_all(code_root.join("apps").join("web").join("src")).expect("src dir");
        std::fs::write(
            code_root.join("apps").join("web").join("package.json"),
            b"{}",
        )
        .expect("package json");
        std::fs::write(
            code_root
                .join("apps")
                .join("web")
                .join("src")
                .join("index.ts"),
            b"export {}\n",
        )
        .expect("source file");
        let detector =
            crate::workspace::WorkspaceMutationDetector::new(&code_root).expect("detector");
        let db_path = temp.root().join(".state").join("local.sqlite3");

        let output = initialize_root(InitOptions {
            db_path: Some(db_path.clone()),
            requested_root: Some(code_root.display().to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        })
        .expect("init succeeds");

        detector.assert_unchanged().expect("workspace unchanged");
        assert_eq!(
            serde_json::to_value(output.root_choice).unwrap(),
            "explicit-existing"
        );
        assert!(output.observed_only);
        assert!(output.scan_summary.workspace_sync_path_count >= 2);

        let store = MetadataStore::open(db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let observed = store
            .observed_path(&workspace_id, "apps/web/src/index.ts")
            .expect("query")
            .expect("observed path");
        assert_eq!(observed.classification, PathClassification::WorkspaceSync);
        assert!(
            store
                .current_namespace_entry(
                    &workspace_id,
                    &WorkspaceRelativePath::new("apps/web/src/index.ts"),
                )
                .expect("current namespace query")
                .is_none(),
            "an observation-only init must not manufacture committed namespace authority"
        );
    }

    #[test]
    fn init_scopes_project_ids_per_workspace_when_reinitializing_same_root() {
        let temp =
            crate::workspace::TempWorkspace::new("init-reworkspace-projects").expect("workspace");
        let code_root = temp.root().join("Code");
        std::fs::create_dir_all(code_root.join("apps/web/.git")).expect("git marker");
        std::fs::write(code_root.join("apps/web/README.md"), b"hello\n").expect("readme");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let default_workspace = WorkspaceId::new("ws_code");
        let account_workspace = WorkspaceId::new("ws_code_account");

        initialize_root_with_workspace(
            InitOptions {
                db_path: Some(db_path.clone()),
                requested_root: Some(code_root.display().to_string()),
                generated_at: "2026-06-29T04:00:00Z".to_string(),
            },
            default_workspace.clone(),
        )
        .expect("default workspace init");
        initialize_root_with_workspace(
            InitOptions {
                db_path: Some(db_path.clone()),
                requested_root: Some(code_root.display().to_string()),
                generated_at: "2026-06-29T04:01:00Z".to_string(),
            },
            account_workspace.clone(),
        )
        .expect("account workspace init");

        let store = MetadataStore::open(db_path).expect("metadata");
        assert_eq!(
            store
                .project_count(&default_workspace)
                .expect("default projects"),
            1
        );
        assert_eq!(
            store
                .project_count(&account_workspace)
                .expect("account projects"),
            1
        );
    }
}
