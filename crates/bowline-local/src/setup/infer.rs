use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

use super::local_state::PackageManagerIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlan {
    pub project_root: PathBuf,
    pub source: SetupInferenceSource,
    pub commands: Vec<SetupCommandPlan>,
    pub blockers: Vec<SetupBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupInferenceSource {
    Lockfiles,
    Toolchains,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCommandPlan {
    pub manager: String,
    pub lockfile: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub approval_required: bool,
    pub approval_reasons: Vec<String>,
    pub package_manager: PackageManagerIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupBlocker {
    pub message: String,
}

#[derive(Debug)]
pub enum SetupInferenceError {
    Io(io::Error),
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

pub fn infer_setup_plan(
    project_root: impl AsRef<Path>,
) -> Result<Option<SetupPlan>, SetupInferenceError> {
    let project_root = project_root.as_ref();
    if project_root.join(".bowlinesetup").is_file() {
        return Ok(None);
    }

    let package_json = read_package_json(project_root)?;
    let lifecycle_hooks = js_lifecycle_hooks(package_json.as_ref());
    let declared_package_manager = package_json
        .as_ref()
        .and_then(|json| json.get("packageManager"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut commands = Vec::new();
    let mut blockers = Vec::new();
    let tools = SystemToolchainLookup;
    push_toolchain_plan(
        &mut commands,
        &mut blockers,
        project_root,
        package_json.as_ref(),
        &tools,
    );

    push_if_lockfile(
        &mut commands,
        project_root,
        "pnpm-lock.yaml",
        "pnpm",
        &["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"],
        declared_package_manager.clone(),
        &lifecycle_hooks,
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "package-lock.json",
        "npm",
        &["npm", "ci", "--ignore-scripts"],
        declared_package_manager.clone(),
        &lifecycle_hooks,
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "bun.lock",
        "bun",
        &["bun", "install", "--frozen-lockfile", "--ignore-scripts"],
        declared_package_manager.clone(),
        &lifecycle_hooks,
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "bun.lockb",
        "bun",
        &["bun", "install", "--frozen-lockfile", "--ignore-scripts"],
        declared_package_manager.clone(),
        &lifecycle_hooks,
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "uv.lock",
        "uv",
        &["uv", "sync", "--frozen"],
        None,
        &[],
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "Cargo.lock",
        "cargo",
        &["cargo", "fetch", "--locked"],
        None,
        &[],
    );
    push_if_lockfile(
        &mut commands,
        project_root,
        "go.sum",
        "go",
        &["go", "mod", "download"],
        None,
        &[],
    );

    if commands.is_empty() && blockers.is_empty() {
        return Ok(None);
    }

    let source = if commands
        .iter()
        .any(|command| command.lockfile.starts_with("toolchain:"))
    {
        SetupInferenceSource::Toolchains
    } else {
        SetupInferenceSource::Lockfiles
    };

    Ok(Some(SetupPlan {
        project_root: project_root.to_path_buf(),
        source,
        commands,
        blockers,
    }))
}

#[derive(Clone, Copy)]
struct ToolchainInstallRule {
    file: &'static str,
    manager: &'static str,
    command: &'static [&'static str],
    reason: &'static str,
}

const TOOLCHAIN_INSTALL_RULES: &[ToolchainInstallRule] = &[
    ToolchainInstallRule {
        file: "mise.toml",
        manager: "mise",
        command: &["mise", "install"],
        reason: "mise.toml declares project toolchains",
    },
    ToolchainInstallRule {
        file: ".tool-versions",
        manager: "mise",
        command: &["mise", "install"],
        reason: ".tool-versions declares project toolchains",
    },
    ToolchainInstallRule {
        file: ".tool-versions",
        manager: "asdf",
        command: &["asdf", "install"],
        reason: ".tool-versions declares project toolchains",
    },
    ToolchainInstallRule {
        file: "rust-toolchain.toml",
        manager: "rustup",
        command: &["rustup", "show"],
        reason: "rust-toolchain.toml pins the Rust toolchain",
    },
];

fn push_toolchain_plan(
    commands: &mut Vec<SetupCommandPlan>,
    blockers: &mut Vec<SetupBlocker>,
    project_root: &Path,
    package_json: Option<&Value>,
    tools: &dyn ToolchainLookup,
) {
    push_manager_install_rules(commands, project_root, tools);
    push_node_blockers(blockers, project_root, package_json, tools);
    push_python_blockers(blockers, project_root, tools);
}

trait ToolchainLookup {
    fn command_on_path(&self, command: &str) -> bool;
    fn which_command(&self, command: &str) -> Option<PathBuf>;
    fn command_version(&self, command: &str, args: &[&str]) -> Option<String>;
}

struct SystemToolchainLookup;

impl ToolchainLookup for SystemToolchainLookup {
    fn command_on_path(&self, command: &str) -> bool {
        which_command(command).is_some()
    }

    fn which_command(&self, command: &str) -> Option<PathBuf> {
        which_command(command)
    }

    fn command_version(&self, command: &str, args: &[&str]) -> Option<String> {
        command_version(command, args)
    }
}

fn push_manager_install_rules(
    commands: &mut Vec<SetupCommandPlan>,
    project_root: &Path,
    tools: &dyn ToolchainLookup,
) {
    let has_mise = tools.command_on_path("mise");
    let has_asdf = tools.command_on_path("asdf");
    for rule in TOOLCHAIN_INSTALL_RULES {
        if !project_root.join(rule.file).is_file() {
            continue;
        }
        if rule.manager == "asdf" && has_mise {
            continue;
        }
        if rule.manager == "mise" && !has_mise {
            continue;
        }
        if rule.manager == "asdf" && !has_asdf {
            continue;
        }
        if rule.manager == "rustup" && !tools.command_on_path("rustup") {
            continue;
        }
        // `rustup show` is read-like, but rustup resolves and installs the
        // pinned toolchain as a side effect. Keep it approval-gated.
        let command = rule
            .command
            .iter()
            .map(|part| (*part).to_string())
            .collect::<Vec<_>>();
        commands.push(SetupCommandPlan {
            manager: rule.manager.to_string(),
            lockfile: format!("toolchain:{}", rule.file),
            command,
            cwd: project_root.to_path_buf(),
            approval_required: true,
            approval_reasons: vec![format!(
                "{}; Bowline needs local approval before running {}",
                rule.reason, rule.command[0]
            )],
            package_manager: PackageManagerIdentity {
                name: rule.manager.to_string(),
                command: rule.command[0].to_string(),
                declared: Some(rule.file.to_string()),
                resolved_path: tools.which_command(rule.command[0]),
                version: None,
            },
        });
        if rule.file == ".tool-versions" && rule.manager == "mise" {
            break;
        }
    }
}

fn push_node_blockers(
    blockers: &mut Vec<SetupBlocker>,
    project_root: &Path,
    package_json: Option<&Value>,
    tools: &dyn ToolchainLookup,
) {
    let Some(want) = node_version_want(project_root, package_json) else {
        return;
    };
    if tools.command_on_path("mise") || tools.command_on_path("asdf") {
        return;
    }
    let have = tools
        .command_version("node", &["--version"])
        .unwrap_or_else(|| "not found".to_string());
    if version_satisfies(&want, &have) {
        return;
    }
    blockers.push(SetupBlocker {
        message: format!("Node {want} required, {have} found; install mise or set up Node {want}"),
    });
}

fn push_python_blockers(
    blockers: &mut Vec<SetupBlocker>,
    project_root: &Path,
    tools: &dyn ToolchainLookup,
) {
    let Some(want) = python_version_want(project_root) else {
        return;
    };
    if tools.command_on_path("mise") || tools.command_on_path("asdf") {
        return;
    }
    let have = tools
        .command_version("python3", &["--version"])
        .or_else(|| tools.command_version("python", &["--version"]))
        .unwrap_or_else(|| "not found".to_string());
    if version_satisfies(&want, &have) {
        return;
    }
    blockers.push(SetupBlocker {
        message: format!(
            "Python {want} required, {have} found; install mise or set up Python {want}"
        ),
    });
}

fn node_version_want(project_root: &Path, package_json: Option<&Value>) -> Option<String> {
    read_trimmed(project_root.join(".node-version")).or_else(|| {
        package_json
            .and_then(|json| json.get("engines"))
            .and_then(|engines| engines.get("node"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn python_version_want(project_root: &Path) -> Option<String> {
    let pyproject = fs::read_to_string(project_root.join("pyproject.toml")).ok()?;
    pyproject.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("requires-python")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(|value| {
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string()
            })
    })
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn command_version(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn version_satisfies(want: &str, have: &str) -> bool {
    let want = normalize_version_hint(want);
    let have = normalize_version_hint(have);
    !want.is_empty() && !have.is_empty() && have.starts_with(&want)
}

fn normalize_version_hint(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('=')
        .trim_start_matches('^')
        .trim_start_matches('~')
        .trim_start_matches(">=")
        .trim_start_matches('>')
        .trim()
        .to_string()
}

fn which_command(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(command);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

fn push_if_lockfile(
    commands: &mut Vec<SetupCommandPlan>,
    project_root: &Path,
    lockfile: &str,
    manager: &str,
    command: &[&str],
    declared: Option<String>,
    lifecycle_hooks: &[String],
) {
    if !project_root.join(lockfile).is_file() {
        return;
    }

    commands.push(SetupCommandPlan {
        manager: manager.to_string(),
        lockfile: lockfile.to_string(),
        command: command.iter().map(|part| (*part).to_string()).collect(),
        cwd: project_root.to_path_buf(),
        approval_required: !lifecycle_hooks.is_empty(),
        approval_reasons: lifecycle_hooks
            .iter()
            .map(|hook| {
                format!("package.json defines {hook}; inferred restore ignores lifecycle scripts")
            })
            .collect(),
        package_manager: PackageManagerIdentity {
            name: manager.to_string(),
            command: command[0].to_string(),
            declared,
            resolved_path: None,
            version: None,
        },
    });
}

fn read_package_json(project_root: &Path) -> Result<Option<Value>, SetupInferenceError> {
    let path = project_root.join("package.json");
    if !path.is_file() {
        return Ok(None);
    }

    let bytes = fs::read(&path)?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|source| SetupInferenceError::Json { path, source })?;
    Ok(Some(value))
}

fn js_lifecycle_hooks(package_json: Option<&Value>) -> Vec<String> {
    let Some(scripts) = package_json
        .and_then(|json| json.get("scripts"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    ["preinstall", "install", "postinstall"]
        .into_iter()
        .filter(|hook| scripts.contains_key(*hook))
        .map(ToOwned::to_owned)
        .collect()
}

impl fmt::Display for SetupInferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "setup inference failed: {error}"),
            Self::Json { path, source } => {
                write!(
                    formatter,
                    "setup inference could not parse {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for SetupInferenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json { source, .. } => Some(source),
        }
    }
}

impl From<io::Error> for SetupInferenceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::workspace::TempWorkspace;

    use super::*;

    #[test]
    fn toolchain_lookup_emits_approval_gated_manager_before_lockfile() {
        let workspace = TempWorkspace::new("setup-infer-toolchain-unit").expect("workspace");
        workspace
            .write_file("app/mise.toml", b"[tools]\nnode = \"24\"\n")
            .expect("mise toml");
        workspace
            .write_file("app/pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")
            .expect("pnpm lock");
        let tools = FakeToolchainLookup::with_commands([("mise", "/fake/bin/mise")]);
        let package_json = read_package_json(&workspace.root().join("app")).expect("package json");
        let mut commands = Vec::new();
        let mut blockers = Vec::new();

        push_toolchain_plan(
            &mut commands,
            &mut blockers,
            &workspace.root().join("app"),
            package_json.as_ref(),
            &tools,
        );
        push_if_lockfile(
            &mut commands,
            &workspace.root().join("app"),
            "pnpm-lock.yaml",
            "pnpm",
            &["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"],
            None,
            &[],
        );

        assert!(blockers.is_empty());
        assert_eq!(commands[0].lockfile, "toolchain:mise.toml");
        assert_eq!(commands[0].command, vec!["mise", "install"]);
        assert!(commands[0].approval_required);
        assert_eq!(commands[1].lockfile, "pnpm-lock.yaml");
    }

    #[test]
    fn node_version_without_manager_reports_blocker_not_command() {
        let workspace = TempWorkspace::new("setup-infer-node-blocker-unit").expect("workspace");
        workspace
            .write_file("app/.node-version", b"99.0.0\n")
            .expect("node version");
        let tools = FakeToolchainLookup::with_versions([("node", "v24.0.0")]);
        let mut commands = Vec::new();
        let mut blockers = Vec::new();

        push_toolchain_plan(
            &mut commands,
            &mut blockers,
            &workspace.root().join("app"),
            None,
            &tools,
        );

        assert!(commands.is_empty());
        assert_eq!(blockers.len(), 1);
        assert!(blockers[0].message.contains("Node 99.0.0 required"));
        assert!(
            blockers[0]
                .message
                .contains("install mise or set up Node 99.0.0")
        );
    }

    #[derive(Default)]
    struct FakeToolchainLookup {
        commands: BTreeSet<String>,
        paths: BTreeMap<String, PathBuf>,
        versions: BTreeMap<String, String>,
    }

    impl FakeToolchainLookup {
        fn with_commands(entries: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            let mut lookup = Self::default();
            for (command, path) in entries {
                lookup.commands.insert(command.to_string());
                lookup
                    .paths
                    .insert(command.to_string(), PathBuf::from(path));
            }
            lookup
        }

        fn with_versions(entries: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            let mut lookup = Self::default();
            for (command, version) in entries {
                lookup
                    .versions
                    .insert(command.to_string(), version.to_string());
            }
            lookup
        }
    }

    impl ToolchainLookup for FakeToolchainLookup {
        fn command_on_path(&self, command: &str) -> bool {
            self.commands.contains(command)
        }

        fn which_command(&self, command: &str) -> Option<PathBuf> {
            self.paths.get(command).cloned()
        }

        fn command_version(&self, command: &str, _args: &[&str]) -> Option<String> {
            self.versions.get(command).cloned()
        }
    }
}
