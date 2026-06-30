use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bowline_core::commands::{AgentCliCapability, AgentCliName};

const AGENT_CLI_NAMES: [(AgentCliName, &str); 3] = [
    (AgentCliName::Codex, "codex"),
    (AgentCliName::Claude, "claude"),
    (AgentCliName::Cursor, "cursor"),
];

pub fn detect_agent_cli_capabilities() -> Vec<AgentCliCapability> {
    detect_agent_cli_capabilities_in_path(env::var_os("PATH"))
}

fn detect_agent_cli_capabilities_in_path(
    path_env: Option<std::ffi::OsString>,
) -> Vec<AgentCliCapability> {
    let paths = path_env
        .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    AGENT_CLI_NAMES
        .into_iter()
        .map(|(name, binary)| capability_for_binary(name, binary, &paths))
        .collect()
}

fn capability_for_binary(
    name: AgentCliName,
    binary: &str,
    paths: &[PathBuf],
) -> AgentCliCapability {
    match executable_on_path(binary, paths) {
        Some(command) => {
            let support = builtin_support(name).merge(sidecar_support(&command));
            AgentCliCapability {
                name,
                available: true,
                command: Some(command.display().to_string()),
                supports_prompt_file_launch: support.prompt_file,
                supports_stdin_launch: support.stdin,
                supports_cwd_selection: support.cwd,
                supports_noninteractive_execution: support.noninteractive,
                supports_receipt_capture: support.receipt,
                degraded_reason: if support.any_launch_mode() {
                    None
                } else {
                    Some(
                        "CLI is installed, but no safe launch mode has been proven; use copy-prompt."
                            .to_string(),
                    )
                },
            }
        }
        None => AgentCliCapability {
            name,
            available: false,
            command: None,
            supports_prompt_file_launch: false,
            supports_stdin_launch: false,
            supports_cwd_selection: false,
            supports_noninteractive_execution: false,
            supports_receipt_capture: false,
            degraded_reason: Some("CLI is not available on PATH.".to_string()),
        },
    }
}

fn executable_on_path(binary: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .map(|path| path.join(binary))
        .find(|candidate| {
            candidate.is_file()
                && candidate
                    .metadata()
                    .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
        })
}

#[derive(Debug, Clone, Copy, Default)]
struct SidecarSupport {
    prompt_file: bool,
    stdin: bool,
    cwd: bool,
    noninteractive: bool,
    receipt: bool,
}

impl SidecarSupport {
    fn any_launch_mode(self) -> bool {
        self.prompt_file || self.stdin
    }

    fn merge(self, other: Self) -> Self {
        Self {
            prompt_file: self.prompt_file || other.prompt_file,
            stdin: self.stdin || other.stdin,
            cwd: self.cwd || other.cwd,
            noninteractive: self.noninteractive || other.noninteractive,
            receipt: self.receipt || other.receipt,
        }
    }
}

fn builtin_support(name: AgentCliName) -> SidecarSupport {
    match name {
        AgentCliName::Codex => SidecarSupport {
            prompt_file: false,
            stdin: true,
            cwd: true,
            noninteractive: true,
            receipt: true,
        },
        AgentCliName::Claude | AgentCliName::Cursor => SidecarSupport::default(),
    }
}

fn sidecar_support(command: &Path) -> SidecarSupport {
    let Some(file_name) = command.file_name().and_then(|name| name.to_str()) else {
        return SidecarSupport::default();
    };
    let sidecar = command.with_file_name(format!("{file_name}.bowline-agent-capabilities"));
    let Ok(contents) = std::fs::read_to_string(sidecar) else {
        return SidecarSupport::default();
    };
    let tokens = contents
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    SidecarSupport {
        prompt_file: tokens.contains(&"prompt-file"),
        stdin: tokens.contains(&"stdin"),
        cwd: tokens.contains(&"cwd"),
        noninteractive: tokens.contains(&"noninteractive"),
        receipt: tokens.contains(&"receipt"),
    }
}

pub fn parse_cli_name(name: &str) -> Option<AgentCliName> {
    match name {
        "codex" => Some(AgentCliName::Codex),
        "claude" => Some(AgentCliName::Claude),
        "cursor" => Some(AgentCliName::Cursor),
        _ => None,
    }
}

#[cfg(test)]
pub fn safe_launch_supported(capability: &AgentCliCapability) -> bool {
    capability.available
        && (capability.supports_prompt_file_launch || capability.supports_stdin_launch)
        && capability.supports_cwd_selection
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    #[test]
    fn detects_available_non_codex_cli_and_degrades_without_proven_launch_mode() {
        let temp = test_temp_dir("detects_available_non_codex_cli");
        let claude = temp.join("claude");
        fs::write(&claude, "#!/bin/sh\n").expect("fake claude");
        let mut permissions = fs::metadata(&claude).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&claude, permissions).expect("permissions");

        let capabilities = detect_agent_cli_capabilities_in_path(Some(temp.clone().into()));
        let claude = capabilities
            .iter()
            .find(|capability| capability.name == AgentCliName::Claude)
            .expect("claude capability");
        assert!(claude.available);
        assert!(!safe_launch_supported(claude));
        assert!(claude.degraded_reason.is_some());
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.name == AgentCliName::Codex && !capability.available)
        );
    }

    #[test]
    fn codex_has_builtin_safe_launch_support_when_installed() {
        let temp = test_temp_dir("codex_has_builtin_safe_launch_support");
        let codex = temp.join("codex");
        fs::write(&codex, "#!/bin/sh\n").expect("fake codex");
        let mut permissions = fs::metadata(&codex).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&codex, permissions).expect("permissions");

        let capabilities = detect_agent_cli_capabilities_in_path(Some(temp.into()));
        let codex = capabilities
            .iter()
            .find(|capability| capability.name == AgentCliName::Codex)
            .expect("codex capability");
        assert!(codex.available);
        assert!(codex.supports_stdin_launch);
        assert!(codex.supports_cwd_selection);
        assert!(codex.supports_noninteractive_execution);
        assert!(codex.supports_receipt_capture);
        assert_eq!(codex.degraded_reason, None);
        assert!(safe_launch_supported(codex));
    }

    #[test]
    fn sidecar_can_prove_prompt_file_launch_for_fake_adapter() {
        let temp = test_temp_dir("sidecar_can_prove_prompt_file_launch");
        let codex = temp.join("codex");
        fs::write(&codex, "#!/bin/sh\n").expect("fake codex");
        fs::write(
            temp.join("codex.bowline-agent-capabilities"),
            "prompt-file cwd noninteractive receipt",
        )
        .expect("sidecar");
        let mut permissions = fs::metadata(&codex).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&codex, permissions).expect("permissions");

        let capabilities = detect_agent_cli_capabilities_in_path(Some(temp.into()));
        let codex = capabilities
            .iter()
            .find(|capability| capability.name == AgentCliName::Codex)
            .expect("codex capability");
        assert!(codex.supports_prompt_file_launch);
        assert!(codex.supports_cwd_selection);
        assert!(safe_launch_supported(codex));
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "bowline-agent-adapter-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("tempdir");
        path
    }
}
