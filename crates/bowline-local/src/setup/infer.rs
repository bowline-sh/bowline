use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::local_state::PackageManagerIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlan {
    pub project_root: PathBuf,
    pub source: SetupInferenceSource,
    pub commands: Vec<SetupCommandPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupInferenceSource {
    Lockfiles,
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

    if commands.is_empty() {
        return Ok(None);
    }

    Ok(Some(SetupPlan {
        project_root: project_root.to_path_buf(),
        source: SetupInferenceSource::Lockfiles,
        commands,
    }))
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
