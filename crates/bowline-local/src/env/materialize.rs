use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Component, Path},
};

use super::parser::{EnvLineKind, ParsedEnvFile, SecretBytes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvValueUpdate {
    pub source_path: String,
    pub key: String,
    pub occurrence_index: usize,
    pub value: SecretBytes,
}

pub fn materialize_env_text(parsed: &ParsedEnvFile, updates: &[EnvValueUpdate]) -> Vec<u8> {
    let updates = updates_for_source(&parsed.source_path, updates);
    let mut output = Vec::new();
    let mut seen_updates = BTreeMap::new();

    for line in &parsed.lines {
        match &line.kind {
            EnvLineKind::Blank | EnvLineKind::Comment => output.extend_from_slice(&line.raw),
            EnvLineKind::Opaque(opaque) => output.extend_from_slice(opaque.bytes.as_bytes()),
            EnvLineKind::KeyValue(value) => {
                let key = (value.key.as_str(), value.occurrence_index);
                let rendered = updates.get(&key);
                output.extend_from_slice(&value.prefix);
                output.extend_from_slice(
                    rendered
                        .map(|update| update.value.as_bytes())
                        .unwrap_or_else(|| value.value.as_bytes()),
                );
                output.extend_from_slice(&value.suffix);
                seen_updates.insert((value.key.as_str(), value.occurrence_index), true);
            }
        }
        output.extend_from_slice(line.ending.as_bytes());
    }

    for update in updates.values() {
        let key = (update.key.as_str(), update.occurrence_index);
        if !seen_updates.contains_key(&key) {
            output.extend_from_slice(update.key.as_bytes());
            output.extend_from_slice(b"=");
            output.extend_from_slice(update.value.as_bytes());
            output.extend_from_slice(b"\n");
        }
    }

    output
}

fn updates_for_source<'a>(
    source_path: &str,
    updates: &'a [EnvValueUpdate],
) -> BTreeMap<(&'a str, usize), &'a EnvValueUpdate> {
    updates
        .iter()
        .filter(|update| update.source_path == source_path)
        .map(|update| ((update.key.as_str(), update.occurrence_index), update))
        .collect()
}

#[derive(Debug)]
pub enum EnvMaterializeError {
    Io(io::Error),
    SymlinkDestination,
}

impl fmt::Display for EnvMaterializeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "env materialization failed: {error}"),
            Self::SymlinkDestination => {
                formatter.write_str("env materialization destination is a symlink")
            }
        }
    }
}

impl Error for EnvMaterializeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::SymlinkDestination => None,
        }
    }
}

impl From<io::Error> for EnvMaterializeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn write_owner_only_env_file(path: &Path, bytes: &[u8]) -> Result<(), EnvMaterializeError> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(EnvMaterializeError::SymlinkDestination);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = path.with_extension("bowline-env-tmp");
    remove_file_if_present(&temp)?;
    {
        let mut file = create_owner_only_file(&temp)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    set_owner_only(path)?;
    Ok(())
}

pub fn write_owner_only_env_file_under_root(
    root: &Path,
    relative_path: &Path,
    bytes: &[u8],
) -> Result<(), EnvMaterializeError> {
    validate_normal_relative_path(relative_path)?;
    ensure_relative_parent_dirs_without_symlink(root, relative_path)?;
    write_owner_only_env_file(&root.join(relative_path), bytes)
}

#[cfg(unix)]
fn create_owner_only_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_owner_only_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_normal_relative_path(relative_path: &Path) -> Result<(), EnvMaterializeError> {
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        return Err(EnvMaterializeError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "env materialization path must be relative",
        )));
    }
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(EnvMaterializeError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "env materialization path must use normal relative components",
        )));
    }
    Ok(())
}

fn ensure_relative_parent_dirs_without_symlink(
    root: &Path,
    relative_path: &Path,
) -> Result<(), EnvMaterializeError> {
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }

    let mut current = root.to_path_buf();
    for component in parent.components() {
        match component {
            Component::Normal(segment) => {
                current.push(segment);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(EnvMaterializeError::SymlinkDestination);
                    }
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => {
                        return Err(EnvMaterializeError::Io(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "env materialization parent is not a directory: {}",
                                current.display()
                            ),
                        )));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&current)?;
                    }
                    Err(error) => return Err(EnvMaterializeError::Io(error)),
                }
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(EnvMaterializeError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "env materialization parent must use normal relative components",
                )));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> io::Result<()> {
    Ok(())
}
