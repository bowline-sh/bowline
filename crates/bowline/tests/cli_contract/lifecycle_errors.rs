use super::*;

struct LifecycleFixture {
    _temp: TempWorkspace,
    project_path: String,
    envs: Vec<(&'static str, String)>,
}

impl LifecycleFixture {
    fn new(name: &str) -> Self {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let home = temp.root().join("home");
        let xdg_state_home = temp.root().join("xdg-state");
        let code_root = home.join("Code");
        let project_path = code_root.join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project directory");
        fs::write(
            project_path.join("src/index.ts"),
            "export const ready = true;\n",
        )
        .expect("project file");
        let db_path =
            database_path_for_platform(current_test_platform(), &home, Some(&xdg_state_home));
        seed_workspace_for_work_views(&db_path, &code_root);
        let envs = vec![
            ("HOME", home.display().to_string()),
            ("XDG_STATE_HOME", xdg_state_home.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-10T12:00:00Z".to_string()),
        ];
        Self {
            _temp: temp,
            project_path: "apps/web".to_string(),
            envs,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        run_bowline_with_env(args, &self.envs)
    }
}

#[test]
fn lifecycle_state_errors_are_user_action_required() {
    let fixture = LifecycleFixture::new("cli-lifecycle-user-action");
    for (args, expected_code) in [
        (
            vec!["forget-local", fixture.project_path.as_str(), "--json"],
            "confirmation_required",
        ),
        (
            vec!["archive", "apps/missing", "--json"],
            "project_not_found",
        ),
        (
            vec!["purge", fixture.project_path.as_str(), "--json"],
            "invalid_lifecycle_state",
        ),
    ] {
        let output = fixture.run(&args);
        assert_lifecycle_error(output, 4, "user-action", expected_code);
    }
}

#[test]
fn lifecycle_metadata_failure_is_retryable() {
    let temp = TempWorkspace::new("cli-lifecycle-runtime").expect("temp workspace");
    let home = temp.root().join("home");
    let xdg_state_home = temp.root().join("xdg-state");
    let db_path = database_path_for_platform(current_test_platform(), &home, Some(&xdg_state_home));
    fs::create_dir_all(db_path.parent().expect("database parent")).expect("state directory");
    fs::write(&db_path, "not sqlite").expect("corrupt metadata fixture");
    let envs = [
        ("HOME", home.display().to_string()),
        ("XDG_STATE_HOME", xdg_state_home.display().to_string()),
    ];

    let output = run_bowline_with_env(&["archive", "apps/web", "--json"], &envs);
    assert_lifecycle_error(output, 3, "retry", "lifecycle_failed");
}

fn assert_lifecycle_error(
    output: Output,
    expected_exit: i32,
    expected_recoverability: &str,
    expected_code: &str,
) {
    assert_eq!(output.status.code(), Some(expected_exit), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["recoverability"], expected_recoverability);
    assert_eq!(json["error"]["code"], expected_code);
}

fn current_test_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}
