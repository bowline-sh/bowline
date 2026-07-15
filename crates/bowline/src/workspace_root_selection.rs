use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedWorkspaceRoot {
    pub(crate) root: String,
    pub(crate) source: WorkspaceRootSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRootSource {
    Explicit,
    CurrentDirectory,
    SingleKnownRoot,
}

impl WorkspaceRootSource {
    pub(crate) fn is_inferred(self) -> bool {
        !matches!(self, Self::Explicit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceRootSelectionError {
    ExplicitRootRequired,
    AmbiguousRoots(Vec<String>),
    MetadataUnavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRootSelection {
    explicit_root: Option<String>,
    db_path: Option<PathBuf>,
    cwd: PathBuf,
}

impl WorkspaceRootSelection {
    pub(crate) fn current(explicit_root: Option<String>) -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            explicit_root,
            db_path: runtime::selected_metadata_database_path(),
            cwd,
        }
    }

    pub(crate) fn resolve(self) -> Result<ResolvedWorkspaceRoot, WorkspaceRootSelectionError> {
        if let Some(root) = self.explicit_root {
            return Ok(ResolvedWorkspaceRoot {
                root: io_helpers::resolve_path_from(&self.cwd, root),
                source: WorkspaceRootSource::Explicit,
            });
        }

        let Some(db_path) = self.db_path else {
            return Err(WorkspaceRootSelectionError::ExplicitRootRequired);
        };
        if !db_path.exists() {
            return Err(WorkspaceRootSelectionError::ExplicitRootRequired);
        }
        let store = MetadataStore::open(db_path)
            .map_err(|error| WorkspaceRootSelectionError::MetadataUnavailable(error.to_string()))?;
        let cwd = self.cwd.display().to_string();
        if let Some(root) = accepted_root_for_path(&store, &cwd)
            .map_err(|error| WorkspaceRootSelectionError::MetadataUnavailable(error.to_string()))?
        {
            return Ok(ResolvedWorkspaceRoot {
                root,
                source: WorkspaceRootSource::CurrentDirectory,
            });
        }

        match bowline_local::metadata::all_accepted_roots(&store)
            .map_err(|error| WorkspaceRootSelectionError::MetadataUnavailable(error.to_string()))?
        {
            roots if roots.is_empty() => Err(WorkspaceRootSelectionError::ExplicitRootRequired),
            mut roots if roots.len() == 1 => Ok(ResolvedWorkspaceRoot {
                root: roots.remove(0),
                source: WorkspaceRootSource::SingleKnownRoot,
            }),
            roots => Err(WorkspaceRootSelectionError::AmbiguousRoots(roots)),
        }
    }

    pub(crate) fn resolve_for_trust(
        self,
    ) -> Result<ResolvedWorkspaceRoot, WorkspaceRootSelectionError> {
        let resolved = self.resolve()?;
        if resolved.source.is_inferred() && runtime::workspace_id_for_root(&resolved.root).is_err()
        {
            return Err(WorkspaceRootSelectionError::ExplicitRootRequired);
        }
        Ok(resolved)
    }
}

fn accepted_root_for_path(
    store: &MetadataStore,
    path: &str,
) -> Result<Option<String>, bowline_local::metadata::MetadataError> {
    let Some(workspace) = store.workspace_by_path(path)? else {
        return Ok(None);
    };
    for root in store.accepted_roots(&workspace.id)? {
        if path_is_under_root(path, &root) {
            return Ok(Some(root));
        }
    }
    store.workspace_root(&workspace.id)
}

fn path_is_under_root(path: &str, root: &str) -> bool {
    let path = service::expand_home_path(path);
    let root = service::expand_home_path(root);
    Path::new(&path).starts_with(Path::new(&root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use bowline_local::workspace::TempWorkspace;

    fn seeded_store(name: &str, roots: &[(&str, &str)]) -> (TempWorkspace, PathBuf, Vec<PathBuf>) {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let mut paths = Vec::new();
        for (index, (workspace, root_name)) in roots.iter().enumerate() {
            let workspace_id = WorkspaceId::new(*workspace);
            let root = temp.root().join(root_name);
            fs::create_dir_all(&root).expect("root exists");
            store
                .insert_workspace(&workspace_id, "User Code", "2026-07-02T12:00:00Z")
                .expect("workspace insert");
            store
                .insert_root(
                    &format!("root_{index}"),
                    &workspace_id,
                    &root.display().to_string(),
                    "2026-07-02T12:00:00Z",
                )
                .expect("root insert");
            paths.push(root);
        }
        (temp, db_path, paths)
    }

    fn selection(
        explicit_root: Option<String>,
        db_path: Option<PathBuf>,
        cwd: PathBuf,
    ) -> WorkspaceRootSelection {
        WorkspaceRootSelection {
            explicit_root,
            db_path,
            cwd,
        }
    }

    #[test]
    fn explicit_root_wins() {
        let temp = TempWorkspace::new("root-selection-explicit").expect("temp workspace");
        let explicit = temp.root().join("Explicit");
        let selected = selection(
            Some(explicit.display().to_string()),
            None,
            temp.root().to_path_buf(),
        )
        .resolve()
        .expect("explicit root resolves");

        assert_eq!(selected.root, explicit.display().to_string());
        assert_eq!(selected.source, WorkspaceRootSource::Explicit);
    }

    #[test]
    fn cwd_under_accepted_root_resolves_that_root() {
        let (_temp, db_path, roots) = seeded_store("root-selection-cwd", &[("ws_code", "Code")]);
        let project = roots[0].join("app");
        fs::create_dir_all(&project).expect("project exists");

        let selected = selection(None, Some(db_path), project)
            .resolve()
            .expect("cwd root resolves");

        assert_eq!(selected.root, roots[0].display().to_string());
        assert_eq!(selected.source, WorkspaceRootSource::CurrentDirectory);
    }

    #[test]
    fn shared_prefix_sibling_is_not_under_root() {
        let temp = TempWorkspace::new("root-selection-prefix-sibling").expect("temp workspace");
        let root = temp.root().join("Code");
        let sibling = temp.root().join("Code2");

        assert!(path_is_under_root(
            &root.join("app").display().to_string(),
            &root.display().to_string(),
        ));
        assert!(!path_is_under_root(
            &sibling.display().to_string(),
            &root.display().to_string(),
        ));
    }

    #[test]
    fn single_known_root_resolves_outside_workspace() {
        let (temp, db_path, roots) = seeded_store("root-selection-known", &[("ws_code", "Code")]);

        let selected = selection(None, Some(db_path), temp.root().join("outside"))
            .resolve()
            .expect("single known root resolves");

        assert_eq!(selected.root, roots[0].display().to_string());
        assert_eq!(selected.source, WorkspaceRootSource::SingleKnownRoot);
    }

    #[test]
    fn multiple_known_roots_are_ambiguous() {
        let (temp, db_path, roots) = seeded_store(
            "root-selection-ambiguous",
            &[("ws_code", "Code"), ("ws_other", "OtherCode")],
        );

        let error = selection(None, Some(db_path), temp.root().join("outside"))
            .resolve()
            .expect_err("multiple roots should be ambiguous");

        assert_eq!(
            error,
            WorkspaceRootSelectionError::AmbiguousRoots(vec![
                roots[1].display().to_string(),
                roots[0].display().to_string(),
            ])
        );
    }
}
