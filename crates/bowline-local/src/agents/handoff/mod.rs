use std::{
    env,
    error::Error,
    fmt, fs,
    io::{self, BufRead, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::commands::{HandoffAgent, HandoffSessionMode};
use serde::{Deserialize, Serialize};

pub mod bundle;
pub mod claude;
pub mod codex;
pub mod tmux;
pub mod transfer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffDiscoveryOptions {
    pub project_path: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
}

impl HandoffDiscoveryOptions {
    pub fn from_project(project_path: PathBuf) -> Self {
        let home = env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
        Self {
            project_path,
            codex_home: env::var_os("BOWLINE_CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            claude_home: env::var_os("BOWLINE_CLAUDE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffSessionCandidate {
    pub agent: HandoffAgent,
    pub session_id: String,
    pub source_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub modified_at_unix_seconds: u64,
    pub sidecars: Vec<TranscriptSidecar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSidecar {
    pub source_path: PathBuf,
    pub install_relative_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTranscript {
    pub agent: HandoffAgent,
    pub source_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffDiscovery {
    pub candidates: Vec<HandoffSessionCandidate>,
    pub skipped: Vec<SkippedTranscript>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHandoff {
    pub mode: HandoffSessionMode,
    pub agent: HandoffAgent,
    pub session: Option<HandoffSessionCandidate>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffSelectError {
    NoSupportedSession,
    NoMatchingSession(String),
    AmbiguousSession(String),
    ConfirmationRequired { default_agent: HandoffAgent },
}

impl fmt::Display for HandoffSelectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSupportedSession => {
                write!(formatter, "no supported Codex or Claude session found")
            }
            Self::NoMatchingSession(session) => {
                write!(formatter, "no matching handoff session `{session}`")
            }
            Self::AmbiguousSession(session) => {
                write!(
                    formatter,
                    "handoff session `{session}` matched multiple candidates"
                )
            }
            Self::ConfirmationRequired { default_agent } => write!(
                formatter,
                "Codex and Claude sessions were detected; confirm default {default_agent:?} handoff"
            ),
        }
    }
}

impl Error for HandoffSelectError {}

pub fn discover_sessions(options: &HandoffDiscoveryOptions) -> HandoffDiscovery {
    let mut codex = codex::discover(options);
    let claude = claude::discover(options);
    codex.candidates.extend(claude.candidates);
    codex.skipped.extend(claude.skipped);
    sort_candidates(&mut codex.candidates);
    codex
}

pub fn select_handoff(
    discovery: &HandoffDiscovery,
    explicit_agent: Option<HandoffAgent>,
    explicit_session: Option<&str>,
    prompt: Option<String>,
    require_confirmation: bool,
) -> Result<SelectedHandoff, HandoffSelectError> {
    if let Some(session) = explicit_session {
        let mut matches = discovery
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.session_id == session
                    || candidate.source_path == Path::new(session)
                    || candidate.source_path.display().to_string() == session
            })
            .filter(|candidate| explicit_agent.is_none_or(|agent| candidate.agent == agent))
            .cloned()
            .collect::<Vec<_>>();
        sort_candidates(&mut matches);
        return match matches.as_slice() {
            [candidate] => Ok(SelectedHandoff {
                mode: HandoffSessionMode::ResumeExisting,
                agent: candidate.agent,
                session: Some(candidate.clone()),
                prompt: None,
            }),
            [] => Err(HandoffSelectError::NoMatchingSession(session.to_string())),
            _ => Err(HandoffSelectError::AmbiguousSession(session.to_string())),
        };
    }

    if let Some(prompt) = prompt {
        let agent = explicit_agent
            .or_else(|| {
                discovery
                    .candidates
                    .first()
                    .map(|candidate| candidate.agent)
            })
            .unwrap_or(HandoffAgent::Codex);
        return Ok(SelectedHandoff {
            mode: HandoffSessionMode::FreshPrompt,
            agent,
            session: None,
            prompt: Some(prompt),
        });
    }

    let mut candidates = discovery
        .candidates
        .iter()
        .filter(|candidate| explicit_agent.is_none_or(|agent| candidate.agent == agent))
        .cloned()
        .collect::<Vec<_>>();
    sort_candidates(&mut candidates);
    let Some(candidate) = candidates.first().cloned() else {
        return Err(HandoffSelectError::NoSupportedSession);
    };

    if explicit_agent.is_none()
        && require_confirmation
        && discovery
            .candidates
            .iter()
            .any(|item| item.agent == HandoffAgent::Codex)
        && discovery
            .candidates
            .iter()
            .any(|item| item.agent == HandoffAgent::Claude)
    {
        return Err(HandoffSelectError::ConfirmationRequired {
            default_agent: candidate.agent,
        });
    }

    Ok(SelectedHandoff {
        mode: HandoffSessionMode::ResumeExisting,
        agent: candidate.agent,
        session: Some(candidate),
        prompt: None,
    })
}

pub(crate) fn sort_candidates(candidates: &mut [HandoffSessionCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .modified_at_unix_seconds
            .cmp(&left.modified_at_unix_seconds)
            .then_with(|| left.agent_label().cmp(right.agent_label()))
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.source_path.cmp(&right.source_path))
    });
}

impl HandoffSessionCandidate {
    fn agent_label(&self) -> &'static str {
        match self.agent {
            HandoffAgent::Codex => "codex",
            HandoffAgent::Claude => "claude",
        }
    }
}

pub(crate) fn modified_seconds(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(system_time_seconds)
        .unwrap_or(0)
}

fn system_time_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

pub(crate) fn discover_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files);
    files.sort();
    files
}

fn collect_jsonl_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_jsonl_files(&path, files);
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

pub(crate) fn metadata_project_matches(candidate: Option<&Path>, requested: &Path) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    normalize_path(candidate) == normalize_path(requested)
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn read_lines_until_metadata(path: &Path, max_lines: usize) -> io::Result<Vec<String>> {
    let file = fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    reader.lines().take(max_lines).collect()
}

pub(crate) fn safe_join(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    if relative.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute handoff install path rejected",
        ));
    }
    let mut out = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(value) => out.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsafe handoff install path rejected",
                ));
            }
        }
    }
    Ok(out)
}

pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_cross_agent_selection_requires_confirmation() {
        let discovery = HandoffDiscovery {
            candidates: vec![
                candidate(HandoffAgent::Codex, "codex_1", 20),
                candidate(HandoffAgent::Claude, "claude_1", 10),
            ],
            skipped: Vec::new(),
        };

        let error = select_handoff(&discovery, None, None, None, true)
            .expect_err("cross-agent default should require confirmation");

        assert_eq!(
            error,
            HandoffSelectError::ConfirmationRequired {
                default_agent: HandoffAgent::Codex
            }
        );
        assert!(select_handoff(&discovery, Some(HandoffAgent::Codex), None, None, true).is_ok());
    }

    #[test]
    fn fresh_prompt_defaults_to_codex_without_prior_sessions() {
        let discovery = HandoffDiscovery {
            candidates: Vec::new(),
            skipped: Vec::new(),
        };

        let selected = select_handoff(&discovery, None, None, Some("continue".to_string()), true)
            .expect("fresh prompt can start without a prior transcript");

        assert_eq!(selected.agent, HandoffAgent::Codex);
        assert_eq!(selected.mode, HandoffSessionMode::FreshPrompt);
        assert_eq!(selected.prompt.as_deref(), Some("continue"));
    }

    #[test]
    fn metadata_reader_returns_only_requested_prefix() {
        let temp =
            std::env::temp_dir().join(format!("bowline-handoff-lines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).expect("temp dir");
        let path = temp.join("session.jsonl");
        fs::write(&path, "one\ntwo\nthree\n").expect("write transcript");

        let lines = read_lines_until_metadata(&path, 2).expect("lines");

        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
    }

    fn candidate(
        agent: HandoffAgent,
        session_id: &str,
        modified_at_unix_seconds: u64,
    ) -> HandoffSessionCandidate {
        HandoffSessionCandidate {
            agent,
            session_id: session_id.to_string(),
            source_path: PathBuf::from(format!("/tmp/{session_id}.jsonl")),
            project_path: Some(PathBuf::from("/tmp/project")),
            modified_at_unix_seconds,
            sidecars: Vec::new(),
        }
    }
}
