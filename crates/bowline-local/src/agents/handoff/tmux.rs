use std::path::{Path, PathBuf};

use bowline_core::commands::{HandoffAgent, HandoffSessionMode};

use crate::bootstrap::ssh::{remote_shell_path, shell_quote};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxLaunch {
    pub session_name: String,
    pub launch_command: String,
    pub has_session_command: String,
    pub attach_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxLaunchRequest {
    pub target: String,
    pub agent: HandoffAgent,
    pub session_mode: HandoffSessionMode,
    pub session_id: Option<String>,
    pub project_path: PathBuf,
    pub prompt_file: Option<PathBuf>,
    pub unique_suffix: String,
}

pub fn render_launch(request: &TmuxLaunchRequest) -> TmuxLaunch {
    let session_name = tmux_session_name(request);
    let agent_command = agent_command(request);
    let quoted_session = shell_quote(&session_name);
    let cd_project = remote_shell_path(&request.project_path.display().to_string());
    let launch_command = format!(
        "tmux new-session -d -s {quoted_session} -c {cd_project} {}",
        shell_quote(&agent_command)
    );
    let has_session_command = format!("tmux has-session -t {quoted_session}");
    let attach_command = format!(
        "ssh {} -t {}",
        shell_quote(&request.target),
        shell_quote(&format!("tmux attach -t {session_name}"))
    );

    TmuxLaunch {
        session_name,
        launch_command,
        has_session_command,
        attach_command,
    }
}

fn tmux_session_name(request: &TmuxLaunchRequest) -> String {
    let agent = match request.agent {
        HandoffAgent::Codex => "codex",
        HandoffAgent::Claude => "claude",
    };
    let project = request
        .project_path
        .file_name()
        .and_then(|value| value.to_str())
        .map(sanitize_tmux_part)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let session_part = request
        .session_id
        .as_deref()
        .map(short_id)
        .unwrap_or_else(|| "prompt".to_string());
    let unique_part = short_unique_suffix(&request.unique_suffix);
    format!("bowline-{agent}-{project}-{session_part}-{unique_part}")
}

fn agent_command(request: &TmuxLaunchRequest) -> String {
    match (request.agent, request.session_mode) {
        (HandoffAgent::Codex, HandoffSessionMode::ResumeExisting) => format!(
            "codex resume {}",
            shell_quote(request.session_id.as_deref().unwrap_or(""))
        ),
        (HandoffAgent::Claude, HandoffSessionMode::ResumeExisting) => format!(
            "claude --resume {}",
            shell_quote(request.session_id.as_deref().unwrap_or(""))
        ),
        (HandoffAgent::Codex, HandoffSessionMode::FreshPrompt) => {
            prompt_file_command("codex", request.prompt_file.as_deref())
        }
        (HandoffAgent::Claude, HandoffSessionMode::FreshPrompt) => {
            prompt_file_command("claude", request.prompt_file.as_deref())
        }
    }
}

fn prompt_file_command(binary: &str, prompt_file: Option<&Path>) -> String {
    match prompt_file {
        Some(path) => format!(
            "{{ cat {}; status=$?; rm -f {}; exit $status; }} | {binary}",
            remote_shell_path(&path.display().to_string()),
            remote_shell_path(&path.display().to_string())
        ),
        None => binary.to_string(),
    }
}

fn short_id(value: &str) -> String {
    sanitize_tmux_part(value).chars().take(12).collect()
}

fn short_unique_suffix(value: &str) -> String {
    let sanitized = sanitize_tmux_part(value);
    let mut chars = sanitized.chars().rev().take(12).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn sanitize_tmux_part(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_resume_command_and_attach_receipt() {
        let launch = render_launch(&TmuxLaunchRequest {
            target: "linux-home".to_string(),
            agent: HandoffAgent::Codex,
            session_mode: HandoffSessionMode::ResumeExisting,
            session_id: Some("sess_123456789".to_string()),
            project_path: PathBuf::from("~/Code/app"),
            prompt_file: None,
            unique_suffix: "now".to_string(),
        });

        assert!(
            launch
                .launch_command
                .contains("bowline-codex-app-sess_1234567-now")
        );
        assert!(launch.launch_command.contains("codex resume"));
        assert!(launch.has_session_command.contains("tmux has-session"));
        assert!(launch.attach_command.contains("tmux attach"));
        assert!(!launch.launch_command.contains("SECRET PROMPT"));
    }

    #[test]
    fn resume_session_name_includes_unique_suffix() {
        let mut request = TmuxLaunchRequest {
            target: "linux-home".to_string(),
            agent: HandoffAgent::Codex,
            session_mode: HandoffSessionMode::ResumeExisting,
            session_id: Some("sess_123456789".to_string()),
            project_path: PathBuf::from("~/Code/app"),
            prompt_file: None,
            unique_suffix: "run-one".to_string(),
        };
        let first = render_launch(&request);
        request.unique_suffix = "run-two".to_string();
        let second = render_launch(&request);

        assert_ne!(first.session_name, second.session_name);
        assert!(first.session_name.contains("sess_123456"));
        assert!(second.session_name.contains("sess_123456"));
    }

    #[test]
    fn prompt_launch_does_not_render_prompt_text() {
        let launch = render_launch(&TmuxLaunchRequest {
            target: "linux-home".to_string(),
            agent: HandoffAgent::Claude,
            session_mode: HandoffSessionMode::FreshPrompt,
            session_id: None,
            project_path: PathBuf::from("~/Code/app"),
            prompt_file: Some(PathBuf::from("~/.cache/bowline/handoff/prompt.txt")),
            unique_suffix: "abc".to_string(),
        });

        assert!(launch.launch_command.contains("prompt.txt"));
        assert!(launch.launch_command.contains("rm -f"));
        assert!(!launch.launch_command.contains("literal prompt"));
    }
}
