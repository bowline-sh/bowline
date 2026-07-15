use std::{cell::RefCell, rc::Rc};

use crate::bootstrap::{
    process::{ProcessError, ProcessOutput, ProcessRunner},
    ssh::{
        BootstrapSshError, BootstrapSshOptions, CodexLaunchPlatform, accept_remote_grant,
        accept_remote_work_view, codex_launch_command, create_remote_agent_lease,
        daemon_status_remote, install_handoff_bundle, launch_handoff_tmux,
        launch_remote_codex_agent, list_remote_devices, prepare_remote_root, probe_remote,
        remote_failure_detail, remove_handoff_prompt_file, start_remote_daemon, status_remote,
        stop_remote_daemon,
    },
};

#[test]
fn remote_failure_uses_bounded_stdout_when_json_errors_leave_stderr_empty() {
    let json_error = format!("{{\"error\":\"{}\"}}", "x".repeat(3_000));

    let detail = remote_failure_detail(&json_error, "");

    assert_eq!(detail.chars().count(), 2_048);
    assert!(detail.starts_with("{\"error\":\""));
    assert_eq!(
        remote_failure_detail(&json_error, " ssh failed \n"),
        "ssh failed"
    );
}

#[derive(Clone)]
struct RecordingRunner {
    args: Rc<RefCell<Vec<String>>>,
    stdin: Rc<RefCell<String>>,
}

impl ProcessRunner for RecordingRunner {
    fn run(&self, _program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
        *self.args.borrow_mut() = args.to_vec();
        *self.stdin.borrow_mut() = String::new();
        Ok(ProcessOutput {
            status_code: 0,
            stdout: "{}".to_string(),
            stderr: String::new(),
        })
    }

    fn run_with_stdin(
        &self,
        _program: &str,
        args: &[String],
        stdin: &str,
    ) -> Result<ProcessOutput, ProcessError> {
        *self.args.borrow_mut() = args.to_vec();
        *self.stdin.borrow_mut() = stdin.to_string();
        Ok(ProcessOutput {
            status_code: 0,
            stdout: "{}".to_string(),
            stderr: String::new(),
        })
    }
}

#[test]
fn remote_probe_prefixes_only_non_secret_bootstrap_environment() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin: stdin.clone(),
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_code".to_string()),
        remote_env: vec![
            (
                "CONVEX_URL".to_string(),
                "https://example.convex.cloud".to_string(),
            ),
            ("BOWLINE_WORKSPACE_ID".to_string(), "ws_code".to_string()),
            (
                "BOWLINE_SECRET_STORE".to_string(),
                "server-local".to_string(),
            ),
        ],
        remote_secret_env: vec![
            (
                "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
                "bowline account session".to_string(),
            ),
            (
                "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN".to_string(),
                "bowline revocation token".to_string(),
            ),
            (
                "BOWLINE_WORKOS_REFRESH_TOKEN".to_string(),
                "workos refresh token".to_string(),
            ),
        ],
        bootstrap_token: Some("scoped bootstrap token".to_string()),
    };

    probe_remote(&runner, &options).expect("probe succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("CONVEX_URL=https://example.convex.cloud"));
    assert!(remote_command.contains("BOWLINE_WORKSPACE_ID=ws_code"));
    assert!(remote_command.contains("BOWLINE_SECRET_STORE=server-local"));
    assert!(remote_command.contains(
        "BOWLINE_METADATA_DB=$HOME/.local/share/bowline/workspaces/ws_code/local.sqlite3"
    ));
    assert!(remote_command.contains("IFS= read -r BOWLINE_BOOTSTRAP_TOKEN"));
    assert!(!remote_command.contains("BOWLINE_CONTROL_PLANE_TOKEN"));
    assert!(!remote_command.contains("scoped bootstrap token"));
    assert!(!remote_command.contains("control token"));
    assert!(!remote_command.contains("bowline account session"));
    assert!(!remote_command.contains("workos refresh token"));
    assert!(remote_command.contains("IFS= read -r BOWLINE_ACCOUNT_SESSION_ID"));
    assert!(remote_command.contains("IFS= read -r BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN"));
    assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_ACCESS_TOKEN"));
    assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_REFRESH_TOKEN"));
    assert!(remote_command.contains("device request --root '~/Code' --json"));
    assert_eq!(
        stdin.borrow().as_str(),
        "scoped bootstrap token\nbowline account session\nbowline revocation token\n"
    );
}

#[test]
fn remote_device_list_uses_explicit_root() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_code".to_string()),
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    list_remote_devices(&runner, &options).expect("list succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("device list --root '~/Code' --json"));
}

#[test]
fn remote_accept_quotes_request_id_before_ssh_execution() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    accept_remote_grant(&runner, &options, "req_1; touch /tmp/pwn").expect("accept succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("device accept --root "));
    assert!(remote_command.contains("--request 'req_1; touch /tmp/pwn' --json"));
    assert!(!remote_command.contains("--request req_1;"));
}

#[test]
fn remote_work_view_accept_uses_canonical_command_path() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin: Rc::new(RefCell::new(String::new())),
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    accept_remote_work_view(&runner, &options, "auth fix").expect("accept succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("work accept 'auth fix' --json"));
}

#[test]
fn remote_probe_rejects_option_like_host_before_ssh_execution() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "-oProxyCommand=touch /tmp/pwn".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    let error = probe_remote(&runner, &options).expect_err("host is rejected");

    assert!(matches!(error, BootstrapSshError::InvalidHost(_)));
    assert!(args.borrow().is_empty());
}

#[test]
fn remote_prepare_and_daemon_commands_use_installed_binary_and_root() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code/agent project".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_agent".to_string()),
        remote_env: vec![(
            "BOWLINE_SECRET_STORE".to_string(),
            "server-local".to_string(),
        )],
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    prepare_remote_root(&runner, &options).expect("prepare succeeds");
    let prepare_args = args.borrow().clone();
    let prepare_command = prepare_args.last().expect("ssh command is recorded");
    assert!(!prepare_command.starts_with("cd "));
    assert!(prepare_command.contains("'~/Code/agent project'"));
    assert!(prepare_command.contains(
        "BOWLINE_METADATA_DB=$HOME/.local/share/bowline/workspaces/ws_agent/local.sqlite3"
    ));
    assert!(
        prepare_command
            .contains("$HOME/.local/bin/bowline setup --root '~/Code/agent project' --json")
    );
    assert!(prepare_command.contains("BOWLINE_SECRET_STORE=server-local"));

    start_remote_daemon(&runner, &options).expect("daemon start succeeds");
    let start_args = args.borrow().clone();
    let start_command = start_args.last().expect("ssh command is recorded");
    assert!(!start_command.starts_with("cd "));
    assert!(start_command.contains("$HOME/.local/bin/bowline daemon start --json"));

    stop_remote_daemon(&runner, &options).expect("daemon stop succeeds");
    let stop_args = args.borrow().clone();
    let stop_command = stop_args.last().expect("ssh command is recorded");
    assert!(!stop_command.starts_with("cd "));
    assert!(stop_command.contains("$HOME/.local/bin/bowline daemon stop --json"));

    daemon_status_remote(&runner, &options).expect("daemon status succeeds");
    let daemon_status_args = args.borrow().clone();
    let daemon_status_command = daemon_status_args.last().expect("ssh command is recorded");
    assert!(!daemon_status_command.starts_with("cd "));
    assert!(daemon_status_command.contains("$HOME/.local/bin/bowline daemon status --json"));
}

#[test]
fn remote_status_uses_explicit_root_without_requiring_root_cwd() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code/new machine".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_agent".to_string()),
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    status_remote(&runner, &options).expect("status succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(!remote_command.starts_with("cd "));
    assert!(
        remote_command
            .contains("$HOME/.local/bin/bowline status --root '~/Code/new machine' --json")
    );
}

#[test]
fn remote_agent_lease_runs_from_root_with_env_on_bowline_command() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_agent".to_string()),
        remote_env: vec![(
            "BOWLINE_SECRET_STORE".to_string(),
            "server-local".to_string(),
        )],
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    create_remote_agent_lease(&runner, &options, "foo", "fix auth")
        .expect("lease command succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains(
        "cd $HOME/Code && BOWLINE_SECRET_STORE=server-local \
             $HOME/.local/bin/bowline agent start foo --task 'fix auth'"
    ));
}
#[test]
fn handoff_installer_pipes_transfer_key_and_envelope_without_exposing_key() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin: stdin.clone(),
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    install_handoff_bundle(
        &runner,
        &options,
        "linux-box",
        "secret transfer key",
        "{\"ciphertext\":\"abc\"}",
    )
    .expect("handoff install succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("BOWLINE_HANDOFF_TARGET=linux-box"));
    assert!(remote_command.contains("IFS= read -r BOWLINE_HANDOFF_TRANSFER_KEY"));
    assert!(remote_command.contains("internal handoff install-bundle --json"));
    assert!(!remote_command.contains("secret transfer key"));
    assert!(!remote_command.contains("ciphertext"));
    assert_eq!(
        stdin.borrow().as_str(),
        "secret transfer key\n{\"ciphertext\":\"abc\"}"
    );
}

#[test]
fn handoff_tmux_launch_runs_launch_then_has_session_in_login_shell() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: None,
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    launch_handoff_tmux(
        &runner,
        &options,
        "tmux new-session -d -s bowline-codex-app-123",
        "tmux has-session -t bowline-codex-app-123",
    )
    .expect("tmux launch succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("bash -lc"));
    assert!(remote_command.contains("tmux new-session -d -s bowline-codex-app-123"));
    assert!(remote_command.contains("&& tmux has-session -t bowline-codex-app-123"));
}

#[test]
fn handoff_prompt_cleanup_removes_quoted_remote_file() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: None,
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    remove_handoff_prompt_file(
        &runner,
        &options,
        std::path::Path::new("/tmp/bowline handoff/prompt.txt"),
    )
    .expect("prompt cleanup succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("rm -f"));
    assert!(remote_command.contains("prompt.txt"));
}

#[test]
fn remote_codex_launch_exports_path_before_bowline_env_prefix() {
    let args = Rc::new(RefCell::new(Vec::new()));
    let stdin = Rc::new(RefCell::new(String::new()));
    let runner = RecordingRunner {
        args: args.clone(),
        stdin,
    };
    let options = BootstrapSshOptions {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        remote_binary: Some("~/.local/bin/bowline".to_string()),
        remote_platform: Some("linux".to_string()),
        remote_workspace_id: Some("ws_agent".to_string()),
        remote_env: vec![
            ("BOWLINE_WORKSPACE_ID".to_string(), "ws_agent".to_string()),
            (
                "BOWLINE_SECRET_STORE".to_string(),
                "server-local".to_string(),
            ),
        ],
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    launch_remote_codex_agent(&runner, &options, "lease_123", "~/Code/app")
        .expect("codex launch command succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    let path_export = remote_command
        .find("export PATH=\"$HOME/.local/bin:$PATH\";")
        .expect("PATH export is present");
    let env_prefix = remote_command
        .find("BOWLINE_WORKSPACE_ID=")
        .expect("workspace env prefix is present");
    let agent_prompt = remote_command
        .find("agent prompt --lease")
        .expect("agent prompt command is present");
    assert!(
        path_export < env_prefix && env_prefix < agent_prompt,
        "{remote_command}"
    );
    assert!(!remote_command.contains("BOWLINE_SECRET_STORE=server-local export PATH"));
    assert!(!remote_command.contains("Library/Application Support/bowline"));
    assert_eq!(
        remote_command
            .matches("--add-dir ~/.local/share/bowline")
            .count(),
        1
    );
    assert_eq!(
        remote_command
            .matches("--add-dir ~/.local/state/bowline")
            .count(),
        1
    );

    let macos = codex_launch_command(
        "~/.local/bin/bowline",
        "lease_123",
        "~/Code/app",
        CodexLaunchPlatform::Macos,
    );
    assert_eq!(macos.matches("--add-dir ~/.local/share/bowline").count(), 1);
    assert_eq!(macos.matches("--add-dir ~/.local/state/bowline").count(), 1);
    assert!(macos.contains("--add-dir \"$HOME/Library/Application Support/bowline\""));
}
