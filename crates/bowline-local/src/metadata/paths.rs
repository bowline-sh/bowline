use std::{
    env, io,
    path::{Path, PathBuf},
};

use super::DEFAULT_DATABASE_FILE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
    Other,
}

pub fn default_database_path() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let xdg_state_home = env::var_os("XDG_STATE_HOME").map(PathBuf::from);

    Ok(database_path_for_platform(
        current_platform(),
        &home,
        xdg_state_home.as_deref(),
    ))
}

pub fn database_path_for_platform(
    platform: Platform,
    home: &Path,
    xdg_state_home: Option<&Path>,
) -> PathBuf {
    let base = match platform {
        Platform::Macos => home
            .join("Library")
            .join("Application Support")
            .join("bowline"),
        Platform::Linux => xdg_state_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("bowline"),
        Platform::Other => home.join(".bowline"),
    };

    base.join(DEFAULT_DATABASE_FILE)
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    }
}

#[cfg(test)]
mod tests {
    use super::{Platform, database_path_for_platform};
    use std::path::Path;

    #[test]
    fn macos_path_uses_application_support() {
        assert_eq!(
            database_path_for_platform(Platform::Macos, Path::new("/workspace/theo"), None),
            Path::new("/workspace/theo/Library/Application Support/bowline/local.sqlite3")
        );
    }

    #[test]
    fn linux_path_uses_xdg_state_when_available() {
        assert_eq!(
            database_path_for_platform(
                Platform::Linux,
                Path::new("/workspace-linux/theo"),
                Some(Path::new("/state"))
            ),
            Path::new("/state/bowline/local.sqlite3")
        );
    }

    #[test]
    fn linux_path_falls_back_to_home_local_state() {
        assert_eq!(
            database_path_for_platform(Platform::Linux, Path::new("/workspace-linux/theo"), None),
            Path::new("/workspace-linux/theo/.local/state/bowline/local.sqlite3")
        );
    }
}
