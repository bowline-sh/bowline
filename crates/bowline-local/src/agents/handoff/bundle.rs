use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::commands::{HandoffAgent, HandoffSessionMode};
use serde::{Deserialize, Serialize};

use super::{
    HandoffSessionCandidate, SelectedHandoff, create_private_dir, safe_join, write_private_file,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffBundle {
    pub manifest: HandoffBundleManifest,
    pub files: Vec<HandoffBundleFile>,
    pub prompt: Option<HandoffPromptPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffBundleManifest {
    pub agent: HandoffAgent,
    pub session_mode: HandoffSessionMode,
    pub session_id: Option<String>,
    pub remote_project_path: PathBuf,
    pub created_for_target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffBundleFile {
    pub install_relative_path: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffPromptPayload {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffInstallReceipt {
    pub agent: HandoffAgent,
    pub session_mode: HandoffSessionMode,
    pub session_id: Option<String>,
    pub installed_files: Vec<PathBuf>,
    pub prompt_file: Option<PathBuf>,
    pub remote_project_path: PathBuf,
}

pub fn build_bundle(
    selected: &SelectedHandoff,
    target: &str,
    remote_project_path: PathBuf,
) -> io::Result<HandoffBundle> {
    let mut files = Vec::new();
    let session_id = selected
        .session
        .as_ref()
        .map(|session| session.session_id.clone());
    if let Some(session) = selected.session.as_ref() {
        files.push(bundle_file_for_session(session)?);
        for sidecar in &session.sidecars {
            files.push(HandoffBundleFile {
                install_relative_path: sidecar.install_relative_path.clone(),
                bytes: fs::read(&sidecar.source_path)?,
            });
        }
    }

    let prompt = selected.prompt.as_ref().map(|prompt| HandoffPromptPayload {
        bytes: prompt.as_bytes().to_vec(),
    });

    Ok(HandoffBundle {
        manifest: HandoffBundleManifest {
            agent: selected.agent,
            session_mode: selected.mode,
            session_id,
            remote_project_path,
            created_for_target: target.to_string(),
        },
        files,
        prompt,
    })
}

fn bundle_file_for_session(session: &HandoffSessionCandidate) -> io::Result<HandoffBundleFile> {
    Ok(HandoffBundleFile {
        install_relative_path: transcript_install_path(session),
        bytes: fs::read(&session.source_path)?,
    })
}

pub fn transcript_install_path(session: &HandoffSessionCandidate) -> PathBuf {
    match session.agent {
        HandoffAgent::Codex => store_relative_path(&session.source_path, "sessions")
            .unwrap_or_else(|| super::codex::remote_session_relative_path(&session.session_id)),
        HandoffAgent::Claude => store_relative_path(&session.source_path, "projects")
            .unwrap_or_else(|| fallback_claude_relative_path(session)),
    }
}

fn store_relative_path(source_path: &Path, marker: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut found = false;
    for component in source_path.components() {
        let value = component.as_os_str();
        if found {
            out.push(value);
        } else if value == marker {
            found = true;
            out.push(value);
        }
    }
    found.then_some(out)
}

fn fallback_claude_relative_path(session: &HandoffSessionCandidate) -> PathBuf {
    let project_slug = session
        .project_path
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    super::claude::remote_session_relative_path(project_slug, &session.session_id)
}

pub fn install_bundle(
    bundle: &HandoffBundle,
    agent_home: &Path,
    temp_root: &Path,
) -> io::Result<HandoffInstallReceipt> {
    create_private_dir(agent_home)?;
    create_private_dir(temp_root)?;
    let mut installed_files = Vec::new();

    for file in &bundle.files {
        let install_path = safe_join(agent_home, &file.install_relative_path)?;
        reject_symlink_parent(agent_home, &install_path)?;
        write_private_file(&install_path, &file.bytes)?;
        installed_files.push(install_path);
    }

    let prompt_file = match bundle.prompt.as_ref() {
        Some(prompt) => {
            let path = temp_root.join(unique_prompt_file_name());
            write_private_file(&path, &prompt.bytes)?;
            Some(path)
        }
        None => None,
    };

    Ok(HandoffInstallReceipt {
        agent: bundle.manifest.agent,
        session_mode: bundle.manifest.session_mode,
        session_id: bundle.manifest.session_id.clone(),
        installed_files,
        prompt_file,
        remote_project_path: bundle.manifest.remote_project_path.clone(),
    })
}

fn unique_prompt_file_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("prompt-{}-{nanos}.txt", std::process::id())
}

fn reject_symlink_parent(root: &Path, path: &Path) -> io::Result<()> {
    let mut current = root.to_path_buf();
    if current.exists() && current.symlink_metadata()?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff install root cannot be a symlink",
        ));
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff install path escaped root",
        ));
    };
    for component in relative.components() {
        current.push(component);
        if current.exists() && current.symlink_metadata()?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "handoff install path cannot traverse a symlink",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::agents::handoff::{HandoffSessionCandidate, SelectedHandoff};

    #[test]
    fn installs_session_file_byte_exact() {
        let temp = temp_root("bundle-byte-exact");
        let source = temp.join("source/session.jsonl");
        fs::create_dir_all(source.parent().expect("source parent")).expect("source dir");
        let bytes = b"{\"type\":\"session_meta\"}\nSECRET TRANSCRIPT LINE\n";
        fs::write(&source, bytes).expect("source bytes");
        let selected = SelectedHandoff {
            mode: HandoffSessionMode::ResumeExisting,
            agent: HandoffAgent::Codex,
            session: Some(HandoffSessionCandidate {
                agent: HandoffAgent::Codex,
                session_id: "sess_1".to_string(),
                source_path: source,
                project_path: Some(temp.join("project")),
                modified_at_unix_seconds: 1,
                sidecars: Vec::new(),
            }),
            prompt: None,
        };

        let bundle = build_bundle(&selected, "linux", PathBuf::from("~/Code/app")).expect("bundle");
        let receipt = install_bundle(&bundle, &temp.join("remote-codex"), &temp.join("tmp"))
            .expect("install");

        assert_eq!(receipt.installed_files.len(), 1);
        assert_eq!(
            fs::read(&receipt.installed_files[0]).expect("installed"),
            bytes
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        let temp = temp_root("bundle-traversal");
        let bundle = HandoffBundle {
            manifest: HandoffBundleManifest {
                agent: HandoffAgent::Codex,
                session_mode: HandoffSessionMode::ResumeExisting,
                session_id: Some("sess".to_string()),
                remote_project_path: PathBuf::from("~/Code/app"),
                created_for_target: "linux".to_string(),
            },
            files: vec![HandoffBundleFile {
                install_relative_path: PathBuf::from("../bad.jsonl"),
                bytes: b"bad".to_vec(),
            }],
            prompt: None,
        };

        assert!(install_bundle(&bundle, &temp.join("remote"), &temp.join("tmp")).is_err());
    }

    #[test]
    fn preserves_discovered_agent_store_relative_paths() {
        let codex = HandoffSessionCandidate {
            agent: HandoffAgent::Codex,
            session_id: "sess_codex".to_string(),
            source_path: PathBuf::from("agent-home/.codex/sessions/2026/07/05/sess_codex.jsonl"),
            project_path: Some(PathBuf::from("/repo")),
            modified_at_unix_seconds: 1,
            sidecars: Vec::new(),
        };
        let claude = HandoffSessionCandidate {
            agent: HandoffAgent::Claude,
            session_id: "sess_claude".to_string(),
            source_path: PathBuf::from(
                "agent-home/.claude/projects/-agent-home-repo/sess_claude.jsonl",
            ),
            project_path: Some(PathBuf::from("agent-home/repo")),
            modified_at_unix_seconds: 1,
            sidecars: Vec::new(),
        };

        assert_eq!(
            transcript_install_path(&codex),
            PathBuf::from("sessions/2026/07/05/sess_codex.jsonl")
        );
        assert_eq!(
            transcript_install_path(&claude),
            PathBuf::from("projects/-agent-home-repo/sess_claude.jsonl")
        );
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("bowline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }
}
