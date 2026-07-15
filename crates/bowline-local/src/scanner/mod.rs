use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use bowline_core::{
    ids::ProjectId,
    policy::{MaterializationMode, PathClassification},
    status::{GitObserverState, ObservedWorkspaceSummary},
    workspace_graph::{FileExecutability, normalize_workspace_path},
};

use crate::policy::{PathFacts, PathPolicyDecision, UserPolicy, classify_path};
use crate::sync::stat_cache::{FileTimestampNanos, StatFingerprint, path_is_under_any_root};

mod git;
use git::{git_config_has_remote, git_remote_tracking_is_stale, git_untracked_file_count};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub root: PathBuf,
    pub projects: Vec<ProjectObservation>,
    pub paths: Vec<PathObservation>,
    pub summary: ObservedWorkspaceSummary,
}

impl ScanReport {
    pub fn path_observations(&self) -> impl ExactSizeIterator<Item = &PathObservation> {
        self.paths.iter()
    }

    pub fn path_observation(&self, path: &str) -> Option<&PathObservation> {
        self.paths
            .binary_search_by(|observation| observation.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.paths[index])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectObservation {
    pub id: ProjectId,
    pub path: String,
    pub has_git_repo: bool,
    pub has_remote: bool,
    pub stale_remote_tracking: bool,
    pub untracked_file_count: u64,
    pub observer_state: GitObserverState,
    /// True when expensive Git health (the untracked-file walk) was deferred
    /// during a partial scan and the reported `untracked_file_count` is a
    /// placeholder that a later full/verify scan must recompute.
    pub health_refresh_needed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathObservation {
    pub path: String,
    pub project_id: Option<ProjectId>,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub byte_len: Option<u64>,
    pub stat: Option<StatFingerprint>,
    pub executability: FileExecutability,
    pub policy: PathPolicyDecision,
}

#[derive(Debug)]
pub enum ScanError {
    Io(io::Error),
}

pub fn scan_workspace(root: impl Into<PathBuf>) -> Result<ScanReport, ScanError> {
    let root = root.into();
    scan_workspace_with_checkpoint(root, || Ok(()))
}

pub fn scan_workspace_with_checkpoint(
    root: impl Into<PathBuf>,
    mut checkpoint: impl FnMut() -> Result<(), ScanError>,
) -> Result<ScanReport, ScanError> {
    scan_workspace_with_roots(root.into(), None, &mut checkpoint)
}

pub fn scan_workspace_scoped(
    root: impl Into<PathBuf>,
    subtree_roots: &BTreeSet<String>,
) -> Result<ScanReport, ScanError> {
    let root = root.into();
    if subtree_roots.iter().any(|path| path.is_empty()) {
        return scan_workspace(root);
    }
    scan_workspace_scoped_with_checkpoint(root, subtree_roots, || Ok(()))
}

pub fn scan_workspace_scoped_with_checkpoint(
    root: impl Into<PathBuf>,
    subtree_roots: &BTreeSet<String>,
    mut checkpoint: impl FnMut() -> Result<(), ScanError>,
) -> Result<ScanReport, ScanError> {
    let root = root.into();
    if subtree_roots.iter().any(|path| path.is_empty()) {
        return scan_workspace_with_checkpoint(root, checkpoint);
    }
    scan_workspace_with_roots(root, Some(subtree_roots), &mut checkpoint)
}

/// Scan only the workspace root's direct children plus the root project's cheap
/// identity, without descending into any subdirectory.
///
/// This is the root-shallow tick: policy discovery reads only root-level inputs,
/// project observation runs the cheap identity/classification pass (no recursive
/// Git untracked walk), and the scanner performs a single `read_dir` of the
/// root. Deeper workspace state is supplied by preserved head entries at the
/// call site, not by this pass.
pub fn scan_workspace_root_shallow(root: impl Into<PathBuf>) -> Result<ScanReport, ScanError> {
    scan_workspace_root_shallow_with_checkpoint(root, || Ok(()))
}

pub fn scan_workspace_root_shallow_with_checkpoint(
    root: impl Into<PathBuf>,
    mut checkpoint: impl FnMut() -> Result<(), ScanError>,
) -> Result<ScanReport, ScanError> {
    let root = root.into();
    let policy = UserPolicy::load_root_only(&root)?;
    let mut scanner = Scanner {
        root: root.clone(),
        policy,
        projects: BTreeMap::new(),
        paths: Vec::new(),
        checkpoint: &mut checkpoint,
    };

    scanner.observe_project_cheap(Path::new(""), &root);
    scanner.scan_root_children()?;

    let projects = scanner.projects.into_values().collect::<Vec<_>>();
    scanner
        .paths
        .sort_by(|left, right| left.path.cmp(&right.path));
    let mut report = ScanReport {
        root,
        summary: summarize(&projects, &scanner.paths),
        projects,
        paths: scanner.paths,
    };
    attach_project_ids(&mut report);
    Ok(report)
}

/// Merge a scoped subtree scan with a root-shallow scan into one report for a
/// combined `DirtySubtrees { root_shallow: true }` tick (Plan 06 U7e, KTD-15).
///
/// Ownership is explicit, not a blind union: the scoped pass owns every path
/// under `roots` (including the root directory entry for each dirty root); the
/// root-shallow pass owns every other (root-level) path it observed. The only
/// overlap is a dirty root's own directory entry, which both passes observe —
/// there the scoped observation wins, so a shallow classification never
/// overrides the scoped subtree's view of its root.
pub fn merge_scoped_and_shallow_reports(
    scoped: ScanReport,
    shallow: ScanReport,
    roots: &BTreeSet<String>,
) -> ScanReport {
    let root = scoped.root.clone();
    // The scoped pass only descends under `roots`, so every path it produced is
    // scoped-owned and kept verbatim.
    let mut paths = scoped.paths;
    let scoped_paths = paths
        .iter()
        .map(|observed| observed.path.clone())
        .collect::<BTreeSet<_>>();
    for observed in shallow.paths {
        // Scoped ownership wins for anything under a dirty root, including the
        // root directory entry itself; the shallow copy is dropped.
        if path_is_under_any_root(&observed.path, roots) || scoped_paths.contains(&observed.path) {
            continue;
        }
        paths.push(observed);
    }
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    // Scoped projects (root project + dirty subtrees, full health) win; add only
    // shallow projects the scoped pass did not already observe.
    let mut projects = scoped.projects;
    let scoped_project_paths = projects
        .iter()
        .map(|project| project.path.clone())
        .collect::<BTreeSet<_>>();
    for project in shallow.projects {
        if !scoped_project_paths.contains(&project.path) {
            projects.push(project);
        }
    }
    let mut report = ScanReport {
        root,
        summary: summarize(&projects, &paths),
        projects,
        paths,
    };
    attach_project_ids(&mut report);
    report
}

fn scan_workspace_with_roots(
    root: PathBuf,
    subtree_roots: Option<&BTreeSet<String>>,
    checkpoint: &mut dyn FnMut() -> Result<(), ScanError>,
) -> Result<ScanReport, ScanError> {
    // A scoped tick loads policy bounded to its dirty subtrees; only a full scan
    // pays the recursive whole-workspace `.bowlineignore` discovery (Plan 06
    // boundedness invariant — a deep single-file edit must not restat `~/Code`).
    let policy = match subtree_roots {
        Some(roots) => UserPolicy::load_scoped(&root, roots)?,
        None => UserPolicy::load(&root)?,
    };
    let mut scanner = Scanner {
        root: root.clone(),
        policy,
        projects: BTreeMap::new(),
        paths: Vec::new(),
        checkpoint,
    };

    if let Some(subtree_roots) = subtree_roots {
        scanner.observe_project(Path::new(""), &root, None);
        for subtree in subtree_roots {
            let relative = PathBuf::from(subtree);
            let absolute = root.join(&relative);
            if !absolute.exists() {
                continue;
            }
            scanner.scan_subtree(relative)?;
        }
    } else {
        scanner.scan_dir(PathBuf::new(), false)?;
    }
    let projects = scanner.projects.into_values().collect::<Vec<_>>();
    scanner
        .paths
        .sort_by(|left, right| left.path.cmp(&right.path));
    let mut report = ScanReport {
        root,
        summary: summarize(&projects, &scanner.paths),
        projects,
        paths: scanner.paths,
    };
    attach_project_ids(&mut report);
    Ok(report)
}

impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "workspace scan failed: {error}"),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

struct Scanner<'a> {
    root: PathBuf,
    policy: UserPolicy,
    projects: BTreeMap<String, ProjectObservation>,
    paths: Vec<PathObservation>,
    checkpoint: &'a mut dyn FnMut() -> Result<(), ScanError>,
}

/// Outcome of observing one directory entry: everything the caller needs to
/// decide whether and how to descend, after the entry has been recorded.
struct ObservedChild {
    relative_path: PathBuf,
    is_git_dir: bool,
    should_recurse: bool,
    pruned_by_policy: bool,
}

impl Scanner<'_> {
    fn scan_subtree(&mut self, relative_path: PathBuf) -> Result<(), ScanError> {
        let absolute_path = self.root.join(&relative_path);
        let metadata = crate::fs_access::symlink_metadata(&absolute_path)?;
        let is_dir = metadata.file_type().is_dir();
        let is_symlink = metadata.file_type().is_symlink();
        let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
        let byte_len = if is_dir { None } else { Some(metadata.len()) };
        let stat = stat_fingerprint(is_dir, &metadata);
        let executability = observed_executability(is_dir, is_symlink, &metadata);
        let policy = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir,
                byte_len,
            },
            &self.policy,
        );
        self.paths.push(PathObservation {
            path,
            project_id: None,
            is_dir,
            is_symlink,
            byte_len,
            stat,
            executability,
            policy,
        });
        if is_dir {
            self.scan_dir(relative_path, false)?;
        }
        Ok(())
    }

    fn scan_dir(
        &mut self,
        relative_dir: PathBuf,
        limited_to_includes: bool,
    ) -> Result<(), ScanError> {
        let absolute_dir = self.root.join(&relative_dir);
        let mut entries =
            crate::fs_access::read_dir(&absolute_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let child_names = entries
            .iter()
            .map(|entry| entry.file_name())
            .collect::<BTreeSet<_>>();
        if !limited_to_includes {
            self.observe_project(&relative_dir, &absolute_dir, Some(&child_names));
        }

        for entry in entries {
            (self.checkpoint)()?;
            let child = self.observe_child(&relative_dir, &entry, limited_to_includes)?;

            if child.is_git_dir {
                self.scan_git_dir(&child.relative_path)?;
                continue;
            }

            if child.should_recurse {
                self.scan_dir(
                    child.relative_path,
                    limited_to_includes || child.pruned_by_policy,
                )?;
            }
        }

        Ok(())
    }

    /// Observe every direct child of the workspace root without descending.
    ///
    /// The single owner of per-entry observation logic ([`Self::observe_child`])
    /// is reused here, so the root-shallow pass classifies children identically
    /// to a full scan; it simply never recurses and never runs `scan_git_dir`.
    fn scan_root_children(&mut self) -> Result<(), ScanError> {
        let mut entries = crate::fs_access::read_dir(&self.root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            (self.checkpoint)()?;
            self.observe_child(Path::new(""), &entry, false)?;
        }

        Ok(())
    }

    /// Classify and record one direct directory entry, returning what the caller
    /// needs to decide recursion. This is the shared per-entry observation body
    /// reused by both the recursive `scan_dir` and the non-recursive
    /// `scan_root_children` so policy/executability handling cannot drift.
    fn observe_child(
        &mut self,
        relative_dir: &Path,
        entry: &fs::DirEntry,
        limited_to_includes: bool,
    ) -> Result<ObservedChild, ScanError> {
        let file_type = entry.file_type()?;
        let relative_path = relative_dir.join(entry.file_name());
        let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
        let metadata = crate::fs_access::symlink_metadata(&entry.path())?;
        let is_dir = file_type.is_dir();
        let is_symlink = file_type.is_symlink();
        let byte_len = if is_dir { None } else { Some(metadata.len()) };
        let stat = stat_fingerprint(is_dir, &metadata);
        let executability = observed_executability(is_dir, is_symlink, &metadata);
        let policy = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir,
                byte_len,
            },
            &self.policy,
        );
        let pruned_by_policy = pruned_by_default_policy(&policy);
        let should_recurse = is_dir && should_recurse_path(&policy, &self.policy, &path);
        let should_record =
            !limited_to_includes || !pruned_by_policy || syncs_as_workspace_state(&policy);
        if should_record {
            self.paths.push(PathObservation {
                path,
                project_id: None,
                is_dir,
                is_symlink,
                byte_len,
                stat,
                executability,
                policy,
            });
        }

        Ok(ObservedChild {
            is_git_dir: is_dir && entry.file_name() == ".git",
            should_recurse,
            pruned_by_policy,
            relative_path,
        })
    }

    fn scan_git_dir(&mut self, relative_git_dir: &Path) -> Result<(), ScanError> {
        self.scan_git_tree(relative_git_dir)
    }

    fn scan_git_tree(&mut self, relative_dir: &Path) -> Result<(), ScanError> {
        let absolute_dir = self.root.join(relative_dir);
        let mut entries = match crate::fs_access::read_dir(&absolute_dir) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            (self.checkpoint)()?;
            let file_type = entry.file_type()?;
            let relative_path = relative_dir.join(entry.file_name());
            let metadata = crate::fs_access::symlink_metadata(&entry.path())?;
            let is_dir = file_type.is_dir();
            let is_symlink = file_type.is_symlink();
            let byte_len = if is_dir { None } else { Some(metadata.len()) };
            let stat = stat_fingerprint(is_dir, &metadata);
            let executability = observed_executability(is_dir, is_symlink, &metadata);
            let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
            let policy = classify_path(
                &PathFacts {
                    relative_path: path.clone(),
                    is_dir,
                    byte_len,
                },
                &self.policy,
            );
            self.paths.push(PathObservation {
                path,
                project_id: None,
                is_dir,
                is_symlink,
                byte_len,
                stat,
                executability,
                policy,
            });
            if is_dir {
                self.scan_git_tree(&relative_path)?;
            }
        }
        Ok(())
    }

    fn observe_project(
        &mut self,
        relative_dir: &Path,
        absolute_dir: &Path,
        child_names: Option<&BTreeSet<OsString>>,
    ) {
        self.observe_project_with_depth(
            relative_dir,
            absolute_dir,
            child_names,
            ProjectHealthDepth::FullHealth,
        );
    }

    /// Observe a project's cheap identity/classification only, deferring the
    /// expensive Git untracked walk. Used by the root-shallow tick so a root
    /// `Cargo.toml`/`package.json` still reclassifies the root project without
    /// walking the repo tree.
    fn observe_project_cheap(&mut self, relative_dir: &Path, absolute_dir: &Path) {
        self.observe_project_with_depth(
            relative_dir,
            absolute_dir,
            None,
            ProjectHealthDepth::CheapIdentity,
        );
    }

    fn observe_project_with_depth(
        &mut self,
        relative_dir: &Path,
        absolute_dir: &Path,
        child_names: Option<&BTreeSet<OsString>>,
        depth: ProjectHealthDepth,
    ) {
        let is_project_root = child_names
            .map(dir_has_project_marker)
            .unwrap_or_else(|| is_project_root(absolute_dir));
        if !is_project_root {
            return;
        }

        let path = normalize_workspace_path(&path_to_slash_string(relative_dir));
        let has_git_repo = absolute_dir.join(".git").is_dir();
        let health = if has_git_repo {
            let git_dir = absolute_dir.join(".git");
            read_project_health(&git_dir, absolute_dir, depth)
        } else {
            ProjectHealth::default()
        };
        self.projects
            .entry(path.clone())
            .or_insert_with(|| ProjectObservation {
                id: project_id_for_path(&path),
                path,
                has_git_repo,
                has_remote: health.has_remote,
                stale_remote_tracking: health.stale_remote_tracking,
                untracked_file_count: health.untracked_file_count,
                observer_state: health.observer_state,
                health_refresh_needed: health.health_refresh_needed,
            });
    }
}

/// How much Git/project health `observe_project_with_depth` collects. Cheap
/// identity is allowed in partial scans; full health runs the expensive
/// untracked-file walk and is reserved for full/verify scans.
#[derive(Clone, Copy)]
enum ProjectHealthDepth {
    CheapIdentity,
    FullHealth,
}

#[derive(Default)]
struct ProjectHealth {
    has_remote: bool,
    stale_remote_tracking: bool,
    untracked_file_count: u64,
    observer_state: GitObserverState,
    health_refresh_needed: bool,
}

fn read_project_health(
    git_dir: &Path,
    absolute_dir: &Path,
    depth: ProjectHealthDepth,
) -> ProjectHealth {
    let mut health = ProjectHealth::default();
    match git_config_has_remote(&git_dir.join("config")) {
        Ok(has_remote) => health.has_remote = has_remote,
        Err(_) => {
            health.observer_state =
                GitObserverState::worst(health.observer_state, GitObserverState::Unavailable)
        }
    }
    match git_remote_tracking_is_stale(git_dir) {
        Ok(stale) => health.stale_remote_tracking = stale,
        Err(_) => {
            health.observer_state =
                GitObserverState::worst(health.observer_state, GitObserverState::Unavailable)
        }
    }

    match depth {
        ProjectHealthDepth::CheapIdentity => {
            health.health_refresh_needed = true;
        }
        ProjectHealthDepth::FullHealth => match git_untracked_file_count(absolute_dir) {
            Ok(untracked) => {
                health.untracked_file_count = untracked.count;
                if !untracked.complete {
                    health.observer_state =
                        GitObserverState::worst(health.observer_state, GitObserverState::Partial);
                }
            }
            Err(_) => {
                health.observer_state =
                    GitObserverState::worst(health.observer_state, GitObserverState::Unavailable);
            }
        },
    }

    health
}

fn stat_fingerprint(is_dir: bool, metadata: &fs::Metadata) -> Option<StatFingerprint> {
    if is_dir {
        return None;
    }
    Some(StatFingerprint {
        size: metadata.len(),
        mtime_ns: FileTimestampNanos::new(timestamp_nanos(metadata.mtime(), metadata.mtime_nsec())),
        ctime_ns: FileTimestampNanos::new(timestamp_nanos(metadata.ctime(), metadata.ctime_nsec())),
        inode: metadata.ino(),
        dev: metadata.dev(),
        file_mode: metadata.mode(),
    })
}

fn timestamp_nanos(seconds: i64, nanos: i64) -> i64 {
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn attach_project_ids(report: &mut ScanReport) {
    let mut projects = report.projects.iter().collect::<Vec<_>>();
    projects.sort_by_key(|project| std::cmp::Reverse(project.path.len()));
    for path in &mut report.paths {
        path.project_id = nearest_project_id(&path.path, &projects);
    }
}

fn nearest_project_id(path: &str, projects: &[&ProjectObservation]) -> Option<ProjectId> {
    projects
        .iter()
        .find(|project| project.path.is_empty() || path_has_prefix(path, &project.path))
        .map(|project| project.id.clone())
}

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn summarize(
    projects: &[ProjectObservation],
    paths: &[PathObservation],
) -> ObservedWorkspaceSummary {
    let mut summary = ObservedWorkspaceSummary {
        repo_count: projects
            .iter()
            .filter(|project| project.has_git_repo)
            .count() as u64,
        no_remote_repo_count: projects
            .iter()
            .filter(|project| project.has_git_repo && !project.has_remote)
            .count() as u64,
        stale_remote_tracking_repo_count: projects
            .iter()
            .filter(|project| project.stale_remote_tracking)
            .count() as u64,
        git_partial_project_count: projects
            .iter()
            .filter(|project| project.observer_state == GitObserverState::Partial)
            .count() as u64,
        git_unavailable_project_count: projects
            .iter()
            .filter(|project| project.observer_state == GitObserverState::Unavailable)
            .count() as u64,
        untracked_file_count: projects
            .iter()
            .map(|project| project.untracked_file_count)
            .sum(),
        ..ObservedWorkspaceSummary::default()
    };

    for path in paths {
        summary.record_path(path.policy.classification, path.policy.mode);
    }

    summary
}

fn observed_executability(
    is_dir: bool,
    is_symlink: bool,
    metadata: &fs::Metadata,
) -> FileExecutability {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if !is_dir && !is_symlink && metadata.permissions().mode() & 0o111 != 0 {
            return FileExecutability::Executable;
        }
    }
    #[cfg(not(unix))]
    let _ = (is_dir, is_symlink, metadata);
    // Non-Unix executable inference is intentionally out of scope for the
    // Unix mode-bit contract.
    FileExecutability::Regular
}

fn should_recurse_path(
    decision: &PathPolicyDecision,
    user_policy: &UserPolicy,
    path: &str,
) -> bool {
    if user_policy.has_include_below(path) {
        return true;
    }

    !matches!(
        decision.classification,
        PathClassification::Dependency
            | PathClassification::Generated
            | PathClassification::Cache
            | PathClassification::LocalOnly
            | PathClassification::Blocked
    )
}

fn pruned_by_default_policy(decision: &PathPolicyDecision) -> bool {
    matches!(
        decision.classification,
        PathClassification::Dependency
            | PathClassification::Generated
            | PathClassification::Cache
            | PathClassification::LocalOnly
            | PathClassification::Blocked
    )
}

fn syncs_as_workspace_state(decision: &PathPolicyDecision) -> bool {
    matches!(
        decision.mode,
        MaterializationMode::WorkspaceSync
            | MaterializationMode::EncryptedSync
            | MaterializationMode::Lazy
            | MaterializationMode::ProjectEnv
    )
}

fn is_project_root(path: &Path) -> bool {
    [
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        ".git",
    ]
    .iter()
    .any(|marker| path.join(marker).exists())
}

fn dir_has_project_marker(child_names: &BTreeSet<OsString>) -> bool {
    [
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        ".git",
    ]
    .iter()
    .any(|marker| child_names.contains(&OsString::from(marker)))
}

fn project_id_for_path(path: &str) -> ProjectId {
    if path.is_empty() {
        return ProjectId::new("proj_root");
    }

    let mut id = String::from("proj_");
    for character in path.chars() {
        match character {
            character if character.is_ascii_alphanumeric() => {
                id.push(character.to_ascii_lowercase());
            }
            '/' => id.push('_'),
            '-' => id.push_str("_dash_"),
            '_' => id.push_str("_us_"),
            '.' => id.push_str("_dot_"),
            character => id.push_str(&format!("_x{:x}_", character as u32)),
        }
    }
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    ProjectId::new(id.trim_matches('_').to_string())
}

fn path_to_slash_string(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests;
