use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRecipe {
    pub source_path: PathBuf,
    pub cwd: PathBuf,
    pub recipe_hash: String,
    pub commands: Vec<SetupRecipeCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRecipeCommand {
    pub line_number: usize,
    pub source_line: String,
    pub command: String,
    pub cwd: PathBuf,
}

#[derive(Debug)]
pub enum SetupRecipeError {
    Io(io::Error),
    MissingParent(PathBuf),
    CwdOutsideWorkspace {
        workspace_root: PathBuf,
        cwd: PathBuf,
    },
}

pub fn load_setup_recipe(
    workspace_root: impl AsRef<Path>,
    recipe_path: impl AsRef<Path>,
) -> Result<SetupRecipe, SetupRecipeError> {
    let recipe_path = recipe_path.as_ref();
    let source = fs::read_to_string(recipe_path)?;
    parse_setup_recipe(workspace_root, recipe_path, &source)
}

pub fn parse_setup_recipe(
    workspace_root: impl AsRef<Path>,
    recipe_path: impl AsRef<Path>,
    source: &str,
) -> Result<SetupRecipe, SetupRecipeError> {
    let recipe_path = recipe_path.as_ref().to_path_buf();
    let cwd = recipe_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SetupRecipeError::MissingParent(recipe_path.clone()))?;
    let cwd = validate_setup_cwd(workspace_root, &cwd)?;
    let recipe_hash = stable_recipe_hash(source.as_bytes());
    let commands = source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let command = line.trim();
            if command.is_empty() || command.starts_with('#') {
                return None;
            }

            Some(SetupRecipeCommand {
                line_number: index + 1,
                source_line: line.to_string(),
                command: command.to_string(),
                cwd: cwd.clone(),
            })
        })
        .collect();

    Ok(SetupRecipe {
        source_path: recipe_path,
        cwd,
        recipe_hash,
        commands,
    })
}

pub fn validate_setup_cwd(
    workspace_root: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
) -> Result<PathBuf, SetupRecipeError> {
    let workspace_root = fs::canonicalize(workspace_root.as_ref())?;
    let cwd = fs::canonicalize(cwd.as_ref())?;
    if cwd.starts_with(&workspace_root) {
        Ok(cwd)
    } else {
        Err(SetupRecipeError::CwdOutsideWorkspace {
            workspace_root,
            cwd,
        })
    }
}

fn stable_recipe_hash(bytes: &[u8]) -> String {
    format!("setup_b3_{}", blake3::hash(bytes).to_hex())
}

impl fmt::Display for SetupRecipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "setup recipe failed: {error}"),
            Self::MissingParent(path) => {
                write!(formatter, "setup recipe has no parent: {}", path.display())
            }
            Self::CwdOutsideWorkspace {
                workspace_root,
                cwd,
            } => write!(
                formatter,
                "setup cwd {} is outside accepted workspace {}",
                cwd.display(),
                workspace_root.display()
            ),
        }
    }
}

impl Error for SetupRecipeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::MissingParent(_) | Self::CwdOutsideWorkspace { .. } => None,
        }
    }
}

impl From<io::Error> for SetupRecipeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
