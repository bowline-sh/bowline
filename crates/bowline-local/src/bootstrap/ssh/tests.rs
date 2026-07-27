use std::{cell::RefCell, rc::Rc};

use crate::bootstrap::{
    process::{ProcessError, ProcessOutput, ProcessRunner},
    ssh::{
        BootstrapSshError, BootstrapSshOptions, accept_remote_grant, daemon_status_remote,
        install_remote_daemon_service, list_remote_devices, prepare_remote_root, probe_remote,
        publish_default_metadata, remote_failure_detail, status_remote,
        workspace_key_status_remote,
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
fn remote_key_status_asks_the_cli_and_never_reads_a_secret_file() {
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

    workspace_key_status_remote(&runner, &options, "ws_code").expect("key status succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("device key-status --workspace ws_code --json"));
    assert!(!remote_command.contains("secrets.v1"));
    assert!(!remote_command.contains("grep"));
    assert!(!remote_command.contains("workspaceId"));
}

#[test]
fn remote_key_status_refuses_a_workspace_id_that_cannot_address_remote_state() {
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

    let error = workspace_key_status_remote(&runner, &options, "ws_code; touch /tmp/pwn")
        .expect_err("an unaddressable workspace id is refused");

    assert!(matches!(error, BootstrapSshError::InvalidWorkspaceId(_)));
    assert!(args.borrow().is_empty());
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

    install_remote_daemon_service(&runner, &options).expect("daemon service install succeeds");
    let install_args = args.borrow().clone();
    let install_command = install_args.last().expect("ssh command is recorded");
    assert!(!install_command.starts_with("cd "));
    assert!(install_command.contains("$HOME/.local/bin/bowline daemon install --json"));
    assert!(
        !install_command.contains("daemon start"),
        "bootstrap must not leave an unmanaged daemon competing with the OS service"
    );

    daemon_status_remote(&runner, &options).expect("daemon status succeeds");
    let daemon_status_args = args.borrow().clone();
    let daemon_status_command = daemon_status_args.last().expect("ssh command is recorded");
    assert!(!daemon_status_command.starts_with("cd "));
    assert!(daemon_status_command.contains("$HOME/.local/bin/bowline daemon status --json"));
}

#[test]
fn remote_metadata_persists_daemon_credentials_with_the_workspace_state() {
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
        remote_workspace_id: Some("ws_agent".to_string()),
        remote_env: vec![
            (
                "CONVEX_URL".to_string(),
                "https://example.convex.cloud".to_string(),
            ),
            ("BOWLINE_WORKSPACE_ID".to_string(), "ws_agent".to_string()),
        ],
        remote_secret_env: vec![
            (
                "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
                "bowline_session_fixture".to_string(),
            ),
            (
                "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN".to_string(),
                "bowline_revoke_fixture".to_string(),
            ),
        ],
        bootstrap_token: None,
    };

    publish_default_metadata(&runner, &options).expect("metadata publish succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(
        remote_command.contains("cat > $HOME/.local/share/bowline/workspaces/ws_agent/daemon.env")
    );
    assert!(
        remote_command
            .contains("chmod 600 $HOME/.local/share/bowline/workspaces/ws_agent/daemon.env")
    );
    assert!(!remote_command.contains("cat > \"$dir/daemon.env\""));
    // The shared `daemon_env` writer renders in sorted key order so the file is
    // byte-stable regardless of how callers assembled the pairs.
    assert_eq!(
        stdin.borrow().as_str(),
        "BOWLINE_ACCOUNT_SESSION_ID=bowline_session_fixture\nBOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN=bowline_revoke_fixture\nBOWLINE_WORKSPACE_ID=ws_agent\nCONVEX_URL=https://example.convex.cloud\n"
    );
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

/// The remote reads its credentials from stdin, one line per key. Run without a
/// pipe — by hand from a terminal, or by a runner that dropped the payload —
/// `read` either waits forever or leaves the variable unset and the CLI reports
/// a configuration it cannot explain. Both are refused where the reason is known.
#[test]
fn the_accept_command_refuses_a_stdin_that_cannot_carry_credentials() {
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
        remote_env: Vec::new(),
        remote_secret_env: vec![
            (
                "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
                "bowline_session_fixture".to_string(),
            ),
            (
                "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN".to_string(),
                "bowline_revoke_fixture".to_string(),
            ),
        ],
        bootstrap_token: Some("scoped bootstrap token".to_string()),
    };

    accept_remote_grant(&runner, &options, "req_1").expect("accept succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(
        remote_command.contains("if [ -t 0 ]; then"),
        "{remote_command}"
    );
    assert!(remote_command.contains("exit 78"), "{remote_command}");
    for key in [
        "BOWLINE_BOOTSTRAP_TOKEN",
        "BOWLINE_ACCOUNT_SESSION_ID",
        "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN",
    ] {
        assert!(
            remote_command.contains(&format!("IFS= read -r {key} || {{")),
            "credential {key} must fail fast when its line never arrives: {remote_command}"
        );
    }
    assert_eq!(
        stdin.borrow().as_str(),
        "scoped bootstrap token\nbowline_session_fixture\nbowline_revoke_fixture\n"
    );
}
