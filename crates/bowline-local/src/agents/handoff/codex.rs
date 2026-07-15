use std::path::{Path, PathBuf};

use bowline_core::commands::HandoffAgent;
use serde_json::Value;

use super::{
    HandoffDiscovery, HandoffDiscoveryOptions, HandoffSessionCandidate, SkippedTranscript,
    discover_jsonl_files, metadata_project_matches, modified_seconds, read_lines_until_metadata,
};

const CODEX_METADATA_SCAN_LINES: usize = 24;

pub fn discover(options: &HandoffDiscoveryOptions) -> HandoffDiscovery {
    let root = options.codex_home.join("sessions");
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();

    for path in discover_jsonl_files(&root) {
        match parse_codex_metadata(&path) {
            Some(metadata)
                if metadata_project_matches(
                    metadata.project_path.as_deref(),
                    &options.project_path,
                ) =>
            {
                candidates.push(HandoffSessionCandidate {
                    agent: HandoffAgent::Codex,
                    session_id: metadata.session_id,
                    source_path: path,
                    project_path: metadata.project_path,
                    modified_at_unix_seconds: modified_seconds(&metadata.source_path),
                    sidecars: Vec::new(),
                });
            }
            Some(_) => {}
            None => skipped.push(SkippedTranscript {
                agent: HandoffAgent::Codex,
                source_path: path,
                reason: "missing Codex session metadata".to_string(),
            }),
        }
    }

    super::sort_candidates(&mut candidates);
    HandoffDiscovery {
        candidates,
        skipped,
    }
}

struct CodexMetadata {
    session_id: String,
    project_path: Option<PathBuf>,
    source_path: PathBuf,
}

fn parse_codex_metadata(path: &Path) -> Option<CodexMetadata> {
    let lines = read_lines_until_metadata(path, CODEX_METADATA_SCAN_LINES).ok()?;
    let mut session_id = None;
    let mut cwd = None;

    for line in lines {
        let value: Value = serde_json::from_str(&line).ok()?;
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            let payload = value.get("payload").unwrap_or(&value);
            session_id = payload
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| payload.get("session_id").and_then(Value::as_str))
                .map(ToString::to_string);
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .or_else(|| payload.get("project_path").and_then(Value::as_str))
                .map(PathBuf::from);
            break;
        }
        session_id = session_id.or_else(|| {
            value
                .get("session_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
        cwd = cwd.or_else(|| value.get("cwd").and_then(Value::as_str).map(PathBuf::from));
    }

    Some(CodexMetadata {
        session_id: session_id?,
        project_path: cwd,
        source_path: path.to_path_buf(),
    })
}

pub fn remote_session_relative_path(session_id: &str) -> PathBuf {
    PathBuf::from("sessions").join(format!("{session_id}.jsonl"))
}

pub fn resume_command(session_id: &str) -> Vec<String> {
    vec![
        "codex".to_string(),
        "resume".to_string(),
        session_id.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn discovers_matching_codex_transcript() {
        let temp = temp_root("codex-discover");
        let project = temp.join("project");
        fs::create_dir_all(&project).expect("project dir");
        let transcript = temp.join("codex/sessions/2026/session.jsonl");
        fs::create_dir_all(transcript.parent().expect("parent")).expect("sessions dir");
        fs::write(
            &transcript,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"sess_1\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"message\",\"content\":\"KEEP\"}}\n",
                project.display()
            ),
        )
        .expect("write transcript");

        let discovery = discover(&HandoffDiscoveryOptions {
            project_path: project.clone(),
            codex_home: temp.join("codex"),
            claude_home: temp.join("claude"),
        });

        assert_eq!(discovery.candidates.len(), 1);
        assert_eq!(discovery.candidates[0].session_id, "sess_1");
        assert_eq!(discovery.candidates[0].source_path, transcript);
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("bowline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[allow(dead_code)]
    fn assert_path(_: &Path) {}
}
