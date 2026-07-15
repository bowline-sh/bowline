use std::{
    env, io,
    path::{Path, PathBuf},
};

use super::{DEFAULT_CONTROL_SOCKET_FILE, DEFAULT_DATABASE_FILE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
    Other,
}

pub fn default_database_path() -> io::Result<PathBuf> {
    Ok(default_state_root()?.join(DEFAULT_DATABASE_FILE))
}

pub fn default_control_socket_path() -> io::Result<PathBuf> {
    Ok(default_state_root()?
        .join("runtime")
        .join(DEFAULT_CONTROL_SOCKET_FILE))
}

pub fn default_state_root() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let xdg_state_home = env::var_os("XDG_STATE_HOME").map(PathBuf::from);

    Ok(state_root_for_platform(
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
    state_root_for_platform(platform, home, xdg_state_home).join(DEFAULT_DATABASE_FILE)
}

pub fn control_socket_path_for_platform(
    platform: Platform,
    home: &Path,
    xdg_state_home: Option<&Path>,
) -> PathBuf {
    state_root_for_platform(platform, home, xdg_state_home)
        .join("runtime")
        .join(DEFAULT_CONTROL_SOCKET_FILE)
}

pub fn state_root_for_platform(
    platform: Platform,
    home: &Path,
    xdg_state_home: Option<&Path>,
) -> PathBuf {
    match platform {
        Platform::Macos => home
            .join("Library")
            .join("Application Support")
            .join("bowline"),
        Platform::Linux => xdg_state_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("bowline"),
        Platform::Other => home.join(".bowline"),
    }
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
    use super::{Platform, control_socket_path_for_platform, database_path_for_platform};
    use std::path::Path;

    #[test]
    fn macos_path_uses_application_support() {
        assert_eq!(
            database_path_for_platform(Platform::Macos, Path::new("/workspace/user"), None),
            Path::new("/workspace/user/Library/Application Support/bowline/local.sqlite3")
        );
    }

    #[test]
    fn linux_path_uses_xdg_state_when_available() {
        assert_eq!(
            database_path_for_platform(
                Platform::Linux,
                Path::new("/workspace-linux/user"),
                Some(Path::new("/state"))
            ),
            Path::new("/state/bowline/local.sqlite3")
        );
    }

    #[test]
    fn linux_path_falls_back_to_home_local_state() {
        assert_eq!(
            database_path_for_platform(Platform::Linux, Path::new("/workspace-linux/user"), None),
            Path::new("/workspace-linux/user/.local/state/bowline/local.sqlite3")
        );
    }

    #[test]
    fn control_socket_path_uses_owner_state_runtime_dir() {
        assert_eq!(
            control_socket_path_for_platform(Platform::Macos, Path::new("/workspace/user"), None),
            Path::new(
                "/workspace/user/Library/Application Support/bowline/runtime/bowline-daemon.sock"
            )
        );
        assert_eq!(
            control_socket_path_for_platform(
                Platform::Linux,
                Path::new("/workspace-linux/user"),
                Some(Path::new("/state"))
            ),
            Path::new("/state/bowline/runtime/bowline-daemon.sock")
        );
    }
}
