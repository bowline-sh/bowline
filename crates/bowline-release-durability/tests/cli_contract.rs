use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn durability_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bowline-release-durability")
}

fn run_inherited(kind: &str, file: &File) -> Output {
    let source_descriptor = file.as_raw_fd();
    let mut command = command("3", kind);
    // SAFETY: the closure performs only async-signal-safe descriptor operations
    // between fork and exec. Descriptor 3 is the declared child contract slot.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source_descriptor, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.output().expect("durability helper should start")
}

fn run_without_descriptor() -> Output {
    let mut command = command("3", "file");
    // SAFETY: close is async-signal-safe and makes the missing-slot precondition
    // deterministic between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::close(3);
            Ok(())
        });
    }
    command.output().expect("durability helper should start")
}

fn command(descriptor: &str, kind: &str) -> Command {
    let mut command = Command::new(durability_binary());
    command.args(["sync-inherited-fd", "--kind", kind, "--fd", descriptor]);
    command
}

fn parse(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout should contain one JSON result")
}

fn expected_platform_contract() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin_f_fullfsync_durability_v1"
    } else if cfg!(target_os = "linux") {
        "linux_fsync_durability_v1"
    } else {
        "unsupported_durability_v1"
    }
}

fn assert_contract_shape(value: &Value) {
    let object = value.as_object().expect("result should be an object");
    let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "failureCode",
            "operation",
            "platformContract",
            "result",
            "schemaVersion"
        ]
    );
}

#[test]
fn syncs_inherited_file_descriptor() {
    let directory = tempfile::tempdir().expect("temp directory should be created");
    let path = directory.path().join("receipt.json");
    fs::write(&path, b"release evidence").expect("fixture should be written");
    let file = File::open(path).expect("fixture should open");
    let output = run_inherited("file", &file);
    assert!(output.status.success());
    let value = parse(&output);
    assert_contract_shape(&value);
    assert_eq!(
        value,
        json!({
            "schemaVersion": 1,
            "operation": "sync-inherited-fd",
            "result": "durable",
            "failureCode": null,
            "platformContract": expected_platform_contract(),
        })
    );
}

#[test]
fn syncs_inherited_directory_descriptor() {
    let directory = tempfile::tempdir().expect("temp directory should be created");
    let file = File::open(directory.path()).expect("directory should open");
    let output = run_inherited("directory", &file);
    assert!(output.status.success());
    let value = parse(&output);
    assert_contract_shape(&value);
    assert_eq!(value["result"], "durable");
    assert_eq!(value["platformContract"], expected_platform_contract());
}

#[test]
fn validates_inherited_descriptor_target_type() {
    let directory = tempfile::tempdir().expect("temp directory should be created");
    let file = File::open(directory.path()).expect("directory should open");
    let output = run_inherited("file", &file);
    assert!(!output.status.success());
    let value = parse(&output);
    assert_eq!(value["failureCode"], "wrong_target_type");
}

#[test]
fn refuses_alternate_descriptor_slots() {
    let output = command("4", "file")
        .output()
        .expect("durability helper should start");
    assert!(!output.status.success());
    let value = parse(&output);
    assert_eq!(value["failureCode"], "invalid_invocation");
    let rendered = String::from_utf8(output.stdout).expect("JSON should be UTF-8");
    assert!(!rendered.contains('4'));
}

#[test]
fn missing_inherited_descriptor_is_privacy_safe() {
    let output = run_without_descriptor();
    assert!(!output.status.success());
    let value = parse(&output);
    assert_eq!(value["failureCode"], "inherited_descriptor_unavailable");
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_kind_is_never_reflected() {
    let private_kind = "private-kind-value";
    let output = command("3", private_kind)
        .output()
        .expect("durability helper should start");
    assert!(!output.status.success());
    let value = parse(&output);
    assert_eq!(value["failureCode"], "invalid_invocation");
    let rendered = String::from_utf8(output.stdout).expect("JSON should be UTF-8");
    assert!(!rendered.contains(private_kind));
}

#[test]
fn path_operations_are_not_part_of_the_authority_contract() {
    let output = Command::new(durability_binary())
        .args(["sync-file", "/private/value"])
        .output()
        .expect("durability helper should start");
    assert!(!output.status.success());
    let value = parse(&output);
    assert_eq!(value["operation"], "invalid");
    assert_eq!(value["failureCode"], "invalid_invocation");
    let rendered = String::from_utf8(output.stdout).expect("JSON should be UTF-8");
    assert!(!rendered.contains("/private/value"));
}
