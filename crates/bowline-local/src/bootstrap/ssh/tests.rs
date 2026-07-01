use std::{cell::RefCell, rc::Rc};

use crate::bootstrap::{
    process::{ProcessError, ProcessOutput, ProcessRunner},
    ssh::{
        BootstrapSshError, BootstrapSshOptions, accept_remote_grant, create_remote_agent_lease,
        daemon_status_remote, launch_remote_codex_agent, prepare_remote_root, probe_remote,
        start_remote_daemon, status_remote,
    },
};

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
                "BOWLINE_WORKOS_REFRESH_TOKEN".to_string(),
                "workos refresh token".to_string(),
            ),
        ],
        bootstrap_token: Some("scoped bootstrap token".to_string()),
    };

    probe_remote(&runner, &options).expect("probe succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("CONVEX_URL='https://example.convex.cloud'"));
    assert!(remote_command.contains("BOWLINE_WORKSPACE_ID='ws_code'"));
    assert!(remote_command.contains("BOWLINE_SECRET_STORE='server-local'"));
    assert!(remote_command.contains(
        "BOWLINE_METADATA_DB=$HOME/'.local/share/bowline/workspaces/ws_code/local.sqlite3'"
    ));
    assert!(remote_command.contains("IFS= read -r BOWLINE_BOOTSTRAP_TOKEN"));
    assert!(!remote_command.contains("BOWLINE_CONTROL_PLANE_TOKEN"));
    assert!(!remote_command.contains("scoped bootstrap token"));
    assert!(!remote_command.contains("control token"));
    assert!(!remote_command.contains("bowline account session"));
    assert!(!remote_command.contains("workos refresh token"));
    assert!(remote_command.contains("IFS= read -r BOWLINE_ACCOUNT_SESSION_ID"));
    assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_ACCESS_TOKEN"));
    assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_REFRESH_TOKEN"));
    assert!(remote_command.contains("devices request --root $HOME/'Code' --json"));
    assert_eq!(
        stdin.borrow().as_str(),
        "scoped bootstrap token\nbowline account session\n"
    );
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
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    };

    accept_remote_grant(&runner, &options, "req_1; touch /tmp/pwn").expect("accept succeeds");

    let captured = args.borrow();
    let remote_command = captured.last().expect("ssh command is recorded");
    assert!(remote_command.contains("devices accept 'req_1; touch /tmp/pwn' --json"));
    assert!(!remote_command.contains("devices accept req_1;"));
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
    assert!(prepare_command.contains("$HOME/'Code/agent project'"));
    assert!(prepare_command.contains(
        "BOWLINE_METADATA_DB=$HOME/'.local/share/bowline/workspaces/ws_agent/local.sqlite3'"
    ));
    assert!(
        prepare_command
            .contains("$HOME/'.local/bin/bowline' init $HOME/'Code/agent project' --json")
    );
    assert!(prepare_command.contains("BOWLINE_SECRET_STORE='server-local'"));

    start_remote_daemon(&runner, &options).expect("daemon start succeeds");
    let start_args = args.borrow().clone();
    let start_command = start_args.last().expect("ssh command is recorded");
    assert!(!start_command.starts_with("cd "));
    assert!(start_command.contains("$HOME/'.local/bin/bowline' daemon start --json"));

    daemon_status_remote(&runner, &options).expect("daemon status succeeds");
    let daemon_status_args = args.borrow().clone();
    let daemon_status_command = daemon_status_args.last().expect("ssh command is recorded");
    assert!(!daemon_status_command.starts_with("cd "));
    assert!(daemon_status_command.contains("$HOME/'.local/bin/bowline' daemon status --json"));
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
            .contains("$HOME/'.local/bin/bowline' status $HOME/'Code/new machine' --json")
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
        "cd $HOME/'Code' && BOWLINE_SECRET_STORE='server-local' \
             $HOME/'.local/bin/bowline' agent start 'foo' --task 'fix auth'"
    ));
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
    assert!(!remote_command.contains("BOWLINE_SECRET_STORE='server-local' export PATH"));
}
