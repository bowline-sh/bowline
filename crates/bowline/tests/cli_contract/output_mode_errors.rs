use super::*;

#[test]
fn piped_invalid_quiet_command_emits_json_usage_error() {
    let output = run_bowline(&["status", "--quiet"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "status");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
}

#[test]
fn piped_mutually_exclusive_quiet_and_json_emits_json_usage_error() {
    let output = run_bowline(&["events", "--quiet", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "events");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(
        json["error"]["message"],
        "--quiet cannot be combined with --json or --human"
    );
}

#[test]
fn explicitly_human_invalid_quiet_command_emits_stderr_text() {
    let output = run_bowline(&["status", "--quiet", "--human"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("usage stderr should be utf8");
    assert!(stderr.contains("bowline usage error"), "{stderr}");
    assert!(
        stderr.contains("unknown bowline status option `--quiet`"),
        "{stderr}"
    );
}
