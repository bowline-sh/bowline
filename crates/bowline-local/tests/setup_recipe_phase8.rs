use bowline_local::{
    setup::{
        LocalRegenerateKind, LocalRegenerateOutput, PackageManagerIdentity, SetupInferenceSource,
        collect_receipt_identity_inputs, infer_setup_plan, load_setup_recipe, parse_setup_recipe,
        redact_setup_text, validate_setup_cwd,
    },
    workspace::TempWorkspace,
};

#[test]
fn setup_recipe_ignores_blank_and_comment_lines_and_preserves_source_lines() {
    let workspace = TempWorkspace::new("phase8-recipe").expect("workspace");
    let recipe_path = workspace
        .write_file(
            "app/.bowlinesetup",
            b"\n# install dependencies\n  pnpm install --frozen-lockfile\n\ncargo fetch --locked  \n",
        )
        .expect("recipe");

    let recipe = load_setup_recipe(workspace.root(), &recipe_path).expect("parsed recipe");

    assert_eq!(recipe.commands.len(), 2);
    assert_eq!(recipe.commands[0].line_number, 3);
    assert_eq!(
        recipe.commands[0].source_line,
        "  pnpm install --frozen-lockfile"
    );
    assert_eq!(recipe.commands[0].command, "pnpm install --frozen-lockfile");
    assert_eq!(recipe.commands[1].line_number, 5);
    assert_eq!(recipe.commands[1].command, "cargo fetch --locked");
    assert!(recipe.recipe_hash.starts_with("setup_b3_"));

    let reparsed = parse_setup_recipe(
        workspace.root(),
        &recipe_path,
        "\n# install dependencies\n  pnpm install --frozen-lockfile\n\ncargo fetch --locked  \n",
    )
    .expect("reparsed recipe");
    assert_eq!(recipe.recipe_hash, reparsed.recipe_hash);
}

#[test]
fn setup_recipe_rejects_cwd_outside_workspace() {
    let workspace = TempWorkspace::new("phase8-recipe-root").expect("workspace");
    let outside = TempWorkspace::new("phase8-recipe-outside").expect("outside");

    let error = validate_setup_cwd(workspace.root(), outside.root()).expect_err("outside rejected");
    assert!(error.to_string().contains("outside accepted workspace"));
}

#[test]
fn setup_redactor_covers_env_assignments_tokens_and_home_paths() {
    let home_app = ["", "home", "user", "Code", "app"].join("/");
    let redacted = redact_setup_text(&format!(
        "OPENAI_API_KEY=sk-abcdef1234567890 printf 'ANTHROPIC_API_KEY=sk-ant-api03-abcdef1234567890'; pnpm install --token ghp_abcdefghijklmnopqrstuvwxyz {home_app} eyJabc.def_123.ghi456"
    ));

    assert!(redacted.text.contains("OPENAI_API_KEY=[redacted]"));
    assert!(redacted.text.contains("'ANTHROPIC_API_KEY=[redacted]'"));
    assert!(!redacted.text.contains("sk-abcdef"));
    assert!(!redacted.text.contains("sk-ant-api03"));
    assert!(!redacted.text.contains("ghp_"));
    assert!(!redacted.text.contains(&home_app));
    assert!(redacted.text.contains("~/Code/app"));
    assert!(redacted.rules.contains(&"env-assignments".to_string()));
    assert!(redacted.rules.contains(&"token-looking-values".to_string()));
    assert!(redacted.rules.contains(&"home-paths".to_string()));
}

#[test]
fn setup_inference_prefers_recipe_and_uses_safe_lockfile_commands() {
    let workspace = TempWorkspace::new("phase8-infer-recipe").expect("workspace");
    workspace
        .write_file("app/.bowlinesetup", b"pnpm install\n")
        .expect("recipe");
    workspace
        .write_file("app/pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")
        .expect("lock");

    assert!(
        infer_setup_plan(workspace.root().join("app"))
            .expect("inference")
            .is_none()
    );

    let workspace = TempWorkspace::new("phase8-infer-locks").expect("workspace");
    workspace
        .write_file(
            "app/package.json",
            br#"{"packageManager":"pnpm@10.30.0","scripts":{"preinstall":"node pre.js","build":"vite"}}"#,
        )
        .expect("package");
    workspace
        .write_file("app/pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")
        .expect("pnpm lock");
    workspace
        .write_file("app/package-lock.json", br#"{"lockfileVersion":3}"#)
        .expect("npm lock");
    workspace.write_file("app/bun.lock", b"").expect("bun lock");
    workspace.write_file("app/uv.lock", b"").expect("uv lock");
    workspace
        .write_file("app/Cargo.lock", b"")
        .expect("cargo lock");
    workspace.write_file("app/go.sum", b"").expect("go sum");

    let plan = infer_setup_plan(workspace.root().join("app"))
        .expect("inference")
        .expect("plan");
    assert_eq!(plan.source, SetupInferenceSource::Lockfiles);
    let commands = plan
        .commands
        .iter()
        .map(|command| (command.lockfile.as_str(), command.command.join(" ")))
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        vec![
            (
                "pnpm-lock.yaml",
                "pnpm install --frozen-lockfile --ignore-scripts".to_string()
            ),
            ("package-lock.json", "npm ci --ignore-scripts".to_string()),
            (
                "bun.lock",
                "bun install --frozen-lockfile --ignore-scripts".to_string()
            ),
            ("uv.lock", "uv sync --frozen".to_string()),
            ("Cargo.lock", "cargo fetch --locked".to_string()),
            ("go.sum", "go mod download".to_string()),
        ]
    );
    assert!(plan.commands[0].approval_required);
    assert_eq!(
        plan.commands[0].approval_reasons,
        vec!["package.json defines preinstall; inferred restore ignores lifecycle scripts"]
    );
    assert!(
        plan.commands
            .iter()
            .all(|command| !command.command.join(" ").contains("build"))
    );
}

#[test]
fn setup_receipt_identity_records_local_regenerate_inputs() {
    let workspace = TempWorkspace::new("phase8-local-state").expect("workspace");
    workspace
        .write_file("app/pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")
        .expect("lock");
    workspace
        .write_file("app/.node-version", b"24\n")
        .expect("node version");
    workspace
        .write_file("app/package.json", br#"{"packageManager":"pnpm@10.30.0"}"#)
        .expect("package");

    let identity = collect_receipt_identity_inputs(
        workspace.root().join("app"),
        "default",
        Some("setup_b3_recipe".to_string()),
        Some(PackageManagerIdentity {
            name: "pnpm".to_string(),
            command: "pnpm".to_string(),
            declared: Some("pnpm@10.30.0".to_string()),
            resolved_path: None,
            version: Some("10.30.0".to_string()),
        }),
    )
    .expect("identity");

    assert_eq!(identity.env_profile, "default");
    assert_eq!(identity.recipe_hash.as_deref(), Some("setup_b3_recipe"));
    assert_eq!(identity.lockfiles.len(), 1);
    assert_eq!(identity.lockfiles[0].path, "pnpm-lock.yaml");
    assert!(identity.lockfiles[0].hash.starts_with("b3_"));
    assert_eq!(
        identity
            .toolchains
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![".node-version", "package.json"]
    );

    let output = LocalRegenerateOutput {
        path: "node_modules".to_string(),
        kind: LocalRegenerateKind::Dependency,
        produced_by: "pnpm install --frozen-lockfile --ignore-scripts".to_string(),
    };
    assert_eq!(output.path, "node_modules");
}
