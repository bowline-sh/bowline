use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    ids::ProjectId,
    policy::{MaterializationMode, PathClassification},
    status::ObservedWorkspaceSummary,
    workspace_graph::normalize_workspace_path,
};

use crate::policy::{PathFacts, PathPolicyDecision, UserPolicy, classify_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub root: PathBuf,
    pub projects: Vec<ProjectObservation>,
    pub paths: Vec<PathObservation>,
    pub summary: ObservedWorkspaceSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectObservation {
    pub id: ProjectId,
    pub path: String,
    pub has_git_repo: bool,
    pub has_remote: bool,
    pub stale_remote_tracking: bool,
    pub untracked_file_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathObservation {
    pub path: String,
    pub project_id: Option<ProjectId>,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub byte_len: Option<u64>,
    pub policy: PathPolicyDecision,
}

#[derive(Debug)]
pub enum ScanError {
    Io(io::Error),
}

pub fn scan_workspace(root: impl Into<PathBuf>) -> Result<ScanReport, ScanError> {
    let root = root.into();
    let policy = UserPolicy::load(&root)?;
    let mut scanner = Scanner {
        root: root.clone(),
        policy,
        projects: BTreeMap::new(),
        paths: Vec::new(),
    };

    scanner.scan_dir(PathBuf::new(), false)?;
    let projects = scanner.projects.into_values().collect::<Vec<_>>();
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

struct Scanner {
    root: PathBuf,
    policy: UserPolicy,
    projects: BTreeMap<String, ProjectObservation>,
    paths: Vec<PathObservation>,
}

impl Scanner {
    fn scan_dir(
        &mut self,
        relative_dir: PathBuf,
        limited_to_includes: bool,
    ) -> Result<(), ScanError> {
        let absolute_dir = self.root.join(&relative_dir);
        if !limited_to_includes {
            self.observe_project(&relative_dir, &absolute_dir);
        }

        let mut entries = fs::read_dir(&absolute_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let file_type = entry.file_type()?;
            let relative_path = relative_dir.join(entry.file_name());
            let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
            let metadata = fs::symlink_metadata(entry.path())?;
            let is_dir = file_type.is_dir();
            let is_symlink = file_type.is_symlink();
            let byte_len = if is_dir { None } else { Some(metadata.len()) };
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
                    path: path.clone(),
                    project_id: None,
                    is_dir,
                    is_symlink,
                    byte_len,
                    policy,
                });
            }

            if is_dir && entry.file_name() == ".git" {
                self.scan_git_dir(&relative_path)?;
                continue;
            }

            if should_recurse {
                self.scan_dir(relative_path, limited_to_includes || pruned_by_policy)?;
            }
        }

        Ok(())
    }

    fn scan_git_dir(&mut self, relative_git_dir: &Path) -> Result<(), ScanError> {
        self.scan_git_tree(relative_git_dir)
    }

    fn scan_git_tree(&mut self, relative_dir: &Path) -> Result<(), ScanError> {
        let absolute_dir = self.root.join(relative_dir);
        let mut entries = match fs::read_dir(&absolute_dir) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let file_type = entry.file_type()?;
            let relative_path = relative_dir.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            let is_dir = file_type.is_dir();
            let is_symlink = file_type.is_symlink();
            let byte_len = if is_dir { None } else { Some(metadata.len()) };
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
                policy,
            });
            if is_dir {
                self.scan_git_tree(&relative_path)?;
            }
        }
        Ok(())
    }

    fn observe_project(&mut self, relative_dir: &Path, absolute_dir: &Path) {
        if !is_project_root(absolute_dir) {
            return;
        }

        let path = normalize_workspace_path(&path_to_slash_string(relative_dir));
        let has_git_repo = absolute_dir.join(".git").is_dir();
        let (has_remote, stale_remote_tracking, untracked_file_count) = if has_git_repo {
            let git_dir = absolute_dir.join(".git");
            (
                git_config_has_remote(&git_dir.join("config")).unwrap_or(false),
                git_remote_tracking_is_stale(&git_dir).unwrap_or(false),
                git_untracked_file_count(absolute_dir).unwrap_or(0),
            )
        } else {
            (false, false, 0)
        };
        self.projects
            .entry(path.clone())
            .or_insert_with(|| ProjectObservation {
                id: project_id_for_path(&path),
                path,
                has_git_repo,
                has_remote,
                stale_remote_tracking,
                untracked_file_count,
            });
    }
}

fn attach_project_ids(report: &mut ScanReport) {
    for path in &mut report.paths {
        path.project_id = nearest_project_id(&path.path, &report.projects);
    }
}

fn nearest_project_id(path: &str, projects: &[ProjectObservation]) -> Option<ProjectId> {
    projects
        .iter()
        .filter(|project| project.path.is_empty() || path_has_prefix(path, &project.path))
        .max_by_key(|project| project.path.len())
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
        untracked_file_count: projects
            .iter()
            .map(|project| project.untracked_file_count)
            .sum(),
        ..ObservedWorkspaceSummary::default()
    };

    for path in paths {
        match path.policy.classification {
            PathClassification::Generated => summary.generated_path_count += 1,
            PathClassification::Dependency => summary.dependency_path_count += 1,
            PathClassification::Cache => summary.generated_path_count += 1,
            PathClassification::ProjectEnv => summary.env_file_count += 1,
            PathClassification::Blocked => summary.blocked_path_count += 1,
            _ => {}
        }
        match path.policy.mode {
            MaterializationMode::WorkspaceSync | MaterializationMode::EncryptedSync => {
                summary.workspace_sync_path_count += 1;
            }
            MaterializationMode::LocalOnly | MaterializationMode::Ignore => {
                summary.local_only_path_count += 1;
            }
            _ => {}
        }
    }

    summary
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

fn git_config_has_remote(config: &Path) -> io::Result<bool> {
    match fs::read_to_string(config) {
        Ok(config) => Ok(config
            .lines()
            .any(|line| line.trim_start().starts_with("[remote "))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn git_remote_tracking_is_stale(git_dir: &Path) -> io::Result<bool> {
    let Some(branch) = git_head_branch(git_dir)? else {
        return Ok(false);
    };
    let Some(local) = read_git_ref(git_dir, &format!("refs/heads/{branch}"))? else {
        return Ok(false);
    };
    let remotes = git_remote_names(&git_dir.join("config"))?;
    for remote in remotes {
        let Some(remote_ref) = read_git_ref(git_dir, &format!("refs/remotes/{remote}/{branch}"))?
        else {
            continue;
        };
        return Ok(remote_ref != local);
    }
    Ok(false)
}

fn git_head_branch(git_dir: &Path) -> io::Result<Option<String>> {
    let head = match fs::read_to_string(git_dir.join("HEAD")) {
        Ok(head) => head,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string))
}

fn git_remote_names(config: &Path) -> io::Result<Vec<String>> {
    let config = match fs::read_to_string(config) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(config
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(str::to_string)
        })
        .collect())
}

fn read_git_ref(git_dir: &Path, reference: &str) -> io::Result<Option<String>> {
    // Loose refs are enough for the current advisory signal; parse packed-refs
    // when freshness needs full Git coverage.
    match fs::read_to_string(git_dir.join(reference)) {
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn git_untracked_file_count(repo: &Path) -> io::Result<u64> {
    let tracked = read_git_index_paths(&repo.join(".git").join("index"))?;
    count_untracked_files(repo, Path::new(""), &tracked)
}

fn count_untracked_files(
    repo: &Path,
    relative_dir: &Path,
    tracked: &BTreeSet<String>,
) -> io::Result<u64> {
    let mut count = 0;
    let mut entries = fs::read_dir(repo.join(relative_dir))?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry.file_type()?;
        let relative_path = relative_dir.join(entry.file_name());
        let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
        if path == ".git" || path.starts_with(".git/") {
            continue;
        }

        let metadata = fs::symlink_metadata(entry.path())?;
        let is_dir = file_type.is_dir();
        let decision = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir,
                byte_len: if is_dir { None } else { Some(metadata.len()) },
            },
            &UserPolicy::empty(),
        );

        if is_dir {
            if !pruned_by_default_policy(&decision) {
                count += count_untracked_files(repo, &relative_path, tracked)?;
            }
        } else if !tracked.contains(&path) && syncs_as_workspace_state(&decision) {
            count += 1;
        }
    }

    Ok(count)
}

fn read_git_index_paths(index: &Path) -> io::Result<BTreeSet<String>> {
    let bytes = match fs::read(index) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error),
    };
    if bytes.len() < 12 || &bytes[0..4] != b"DIRC" {
        return Ok(BTreeSet::new());
    }

    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("slice length"));
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().expect("slice length"));
    if !matches!(version, 2 | 3) {
        // Advisory counts only; parse index v4 when Git freshness becomes
        // product-critical.
        return Ok(BTreeSet::new());
    }

    let mut paths = BTreeSet::new();
    let mut offset = 12usize;
    for _ in 0..entry_count {
        if offset + 62 > bytes.len() {
            break;
        }

        let flags = u16::from_be_bytes([bytes[offset + 60], bytes[offset + 61]]);
        let name_start = offset
            + if version >= 3 && flags & 0x4000 != 0 {
                64
            } else {
                62
            };
        let name_len = (flags & 0x0fff) as usize;
        let Some(name_end) = index_name_end(&bytes, name_start, name_len) else {
            break;
        };

        paths.insert(normalize_workspace_path(&String::from_utf8_lossy(
            &bytes[name_start..name_end],
        )));
        offset += (name_end + 1 - offset).div_ceil(8) * 8;
    }

    Ok(paths)
}

fn index_name_end(bytes: &[u8], name_start: usize, name_len: usize) -> Option<usize> {
    if name_len < 0x0fff {
        let end = name_start.checked_add(name_len)?;
        return (end < bytes.len()).then_some(end);
    }

    bytes[name_start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|nul| name_start + nul)
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
mod tests {
    use std::process::Command;

    use super::scan_workspace;

    #[test]
    fn scan_observes_projects_without_mutating_git() {
        let temp = crate::workspace::TempWorkspace::new("scan-git").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        temp.create_git_repo("apps/web").expect("git repo");
        let detector = temp.mutation_detector().expect("mutation detector");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        detector.assert_unchanged().expect("scan should not mutate");
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.summary.repo_count, 1);
        assert_eq!(report.summary.no_remote_repo_count, 1);
        assert!(
            report
                .paths
                .iter()
                .any(|path| path.path == "apps/web/.git/config")
        );
    }

    #[test]
    fn scan_counts_generated_env_and_dependency_paths() {
        let temp = crate::workspace::TempWorkspace::new("scan-policy").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        temp.write_project_file("apps/web", ".env.local", b"SECRET=value\n")
            .expect("env file");
        temp.create_generated_folder("apps/web", ".next")
            .expect("generated folder");
        std::fs::create_dir_all(temp.root().join("apps/web/node_modules/react"))
            .expect("node modules");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        assert_eq!(report.summary.env_file_count, 1);
        assert!(report.summary.generated_path_count >= 1);
        assert!(report.summary.dependency_path_count >= 1);
    }

    #[test]
    fn scan_counts_untracked_files_without_mutating_git() {
        let temp = crate::workspace::TempWorkspace::new("scan-untracked").expect("temp workspace");
        let repo = temp.root().join("apps").join("web");
        std::fs::create_dir_all(&repo).expect("repo dir");
        Command::new("git")
            .arg("init")
            .arg(&repo)
            .output()
            .expect("git init should run");
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["remote", "add", "origin", "git@example.com:acme/web.git"])
            .output()
            .expect("git remote should run");
        std::fs::write(repo.join("package.json"), b"{}").expect("package json");
        std::fs::create_dir_all(repo.join("notes")).expect("notes dir");
        std::fs::write(repo.join("notes").join("repro.md"), b"steps\n").expect("untracked file");
        std::fs::create_dir_all(repo.join("node_modules/react/.git")).expect("nested dependency");
        let detector =
            crate::workspace::WorkspaceMutationDetector::new(&repo).expect("mutation detector");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        detector.assert_unchanged().expect("scan should not mutate");
        assert_eq!(report.summary.repo_count, 1);
        assert_eq!(report.summary.no_remote_repo_count, 0);
        assert!(report.summary.untracked_file_count >= 1);
        assert!(
            report
                .projects
                .iter()
                .all(|project| project.path != "apps/web/node_modules/react")
        );
        assert!(report.paths.iter().any(|path| {
            path.path == "apps/web/notes/repro.md"
                && serde_json::to_value(path.policy.mode).unwrap() == "workspace-sync"
        }));
    }

    #[test]
    fn scan_detects_stale_remote_tracking_refs_without_running_git() {
        let temp =
            crate::workspace::TempWorkspace::new("scan-stale-git-ref").expect("temp workspace");
        let git_dir = temp.root().join("apps/web/.git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
        std::fs::create_dir_all(git_dir.join("refs/remotes/origin")).expect("remote refs");
        std::fs::write(temp.root().join("apps/web/package.json"), b"{}").expect("package");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(git_dir.join("config"), b"[remote \"origin\"]\n").expect("config");
        std::fs::write(git_dir.join("refs/heads/main"), b"aaaaaaaa\n").expect("local ref");
        std::fs::write(git_dir.join("refs/remotes/origin/main"), b"bbbbbbbb\n")
            .expect("remote ref");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        assert_eq!(report.summary.repo_count, 1);
        assert_eq!(report.summary.stale_remote_tracking_repo_count, 1);
    }

    #[test]
    fn scan_observes_bounded_git_transients_as_local_only() {
        let temp =
            crate::workspace::TempWorkspace::new("scan-git-transients").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        let git = temp.create_git_repo("apps/web").expect("git repo");
        std::fs::write(git.join("index.lock"), b"lock").expect("index lock");
        std::fs::write(git.join("gc.log"), b"gc").expect("gc log");
        std::fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
        std::fs::write(git.join("objects").join("pack").join("tmp_pack"), b"tmp")
            .expect("tmp pack");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        for path in [
            "apps/web/.git/index.lock",
            "apps/web/.git/gc.log",
            "apps/web/.git/objects/pack/tmp_pack",
        ] {
            assert!(report.paths.iter().any(|observed| {
                observed.path == path
                    && serde_json::to_value(observed.policy.mode).unwrap() == "local-only"
            }));
        }
        assert!(report.summary.local_only_path_count >= 3);
    }

    #[test]
    fn scan_recurses_git_refs_objects_and_packfiles_as_opaque_workspace_state() {
        let temp =
            crate::workspace::TempWorkspace::new("scan-git-opaque-tree").expect("temp workspace");
        temp.write_project_file("apps/web", "package.json", b"{}")
            .expect("package json");
        let git = temp.create_git_repo("apps/web").expect("git repo");
        std::fs::write(git.join("refs").join("heads").join("main"), b"abc123\n")
            .expect("branch ref");
        std::fs::create_dir_all(git.join("objects").join("ab")).expect("object dir");
        std::fs::write(git.join("objects").join("ab").join("cdef"), b"loose-object")
            .expect("loose object");
        std::fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
        std::fs::write(
            git.join("objects").join("pack").join("pack-main-001.pack"),
            b"pack-bytes",
        )
        .expect("pack file");
        let detector = temp.mutation_detector().expect("mutation detector");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        detector.assert_unchanged().expect("scan should not mutate");
        for path in [
            "apps/web/.git/refs/heads/main",
            "apps/web/.git/objects/ab/cdef",
            "apps/web/.git/objects/pack/pack-main-001.pack",
        ] {
            assert!(report.paths.iter().any(|observed| {
                observed.path == path
                    && serde_json::to_value(observed.policy.mode).unwrap() == "encrypted-sync"
                    && observed.policy.matched_rule == "git-opaque-state"
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_records_dangling_symlink_without_following_it() {
        let temp = crate::workspace::TempWorkspace::new("scan-symlink").expect("temp workspace");
        temp.write_file("package.json", b"{}")
            .expect("package json");
        temp.create_symlink("", "missing-link", "does-not-exist")
            .expect("dangling symlink");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        assert!(report.paths.iter().any(|path| {
            path.path == "missing-link"
                && !path.is_dir
                && serde_json::to_value(path.policy.mode).unwrap() == "workspace-sync"
        }));
    }

    #[test]
    fn scanner_recurses_into_included_dependency_paths() {
        let temp = crate::workspace::TempWorkspace::new("scan-include").expect("temp workspace");
        temp.write_file(".bowlineignore", b"!node_modules/kept.js\n")
            .expect("ignore");
        temp.write_file("package.json", b"{}")
            .expect("package json");
        temp.write_file("node_modules/kept.js", b"module.exports = {}\n")
            .expect("included dependency");
        temp.write_file("node_modules/skipped.js", b"module.exports = {}\n")
            .expect("skipped dependency");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        assert!(report.paths.iter().any(|path| {
            path.path == "node_modules/kept.js"
                && serde_json::to_value(path.policy.mode).unwrap() == "workspace-sync"
        }));
        assert!(
            !report
                .paths
                .iter()
                .any(|path| path.path == "node_modules/skipped.js")
        );
    }

    #[test]
    fn scanner_keeps_work_view_namespace_out_of_canonical_workspace_state() {
        let temp =
            crate::workspace::TempWorkspace::new("scan-work-namespace").expect("temp workspace");
        temp.write_file("package.json", b"{}")
            .expect("package json");
        temp.write_file(".work/app/feature/src/auth.ts", b"work view edit\n")
            .expect("work file");

        let report = scan_workspace(temp.root()).expect("scan succeeds");

        assert!(report.paths.iter().any(|path| {
            path.path == ".work" && serde_json::to_value(path.policy.mode).unwrap() == "local-only"
        }));
        assert!(
            !report
                .paths
                .iter()
                .any(|path| path.path == ".work/app/feature/src/auth.ts"),
            ".work contents must not become canonical workspace paths"
        );
    }

    #[test]
    fn project_ids_do_not_collide_for_punctuation_variants() {
        let temp =
            crate::workspace::TempWorkspace::new("scan-project-ids").expect("temp workspace");
        temp.write_project_file("apps/web-api", "package.json", b"{}")
            .expect("first project");
        temp.write_project_file("apps/web_api", "package.json", b"{}")
            .expect("second project");

        let report = scan_workspace(temp.root()).expect("scan succeeds");
        let ids = report
            .projects
            .iter()
            .map(|project| project.id.as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(report.projects.len(), 2);
        assert_eq!(ids.len(), 2);
    }
}
