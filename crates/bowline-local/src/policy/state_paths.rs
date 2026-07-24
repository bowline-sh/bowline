//! Workspace state-path predicates: which workspace-relative paths belong to
//! Bowline's private engine state, and which paths may carry secrets. Moved
//! here from the deleted old sync engine — `policy` is the single owner of
//! what syncs.

use super::is_project_env_name;

/// Whether any path component is a project `.env` file, so callers can flag
/// content that may carry secrets.
pub fn is_secret_bearing_path(path: &str) -> bool {
    path.split('/').any(is_project_env_name)
}

/// Whether a workspace-relative path belongs to Bowline's private or
/// derivable filesystem state rather than the encrypted Workspace Snapshot.
///
/// The root `.bowline` directory is daemon-owned state. Materialization temp
/// files are excluded at any depth because a crash may leave them behind, but
/// they never represent user authority. All other ordinary workspace state —
/// including `.env`, opaque `.git`, and similarly named paths — remains sync
/// input.
pub fn is_private_workspace_state_path(path: &str) -> bool {
    path == ".bowline"
        || path.starts_with(".bowline/")
        || path
            .split('/')
            .any(|part| part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_secret_bearing_paths() {
        assert!(is_secret_bearing_path(".env"));
        assert!(is_secret_bearing_path("apps/web/.env.local"));
        assert!(is_secret_bearing_path("service.env"));
        assert!(!is_secret_bearing_path("src/env_reader.rs"));
    }

    #[test]
    fn private_workspace_state_excludes_only_bowline_state_and_derivable_temps() {
        for path in [
            ".bowline",
            ".bowline/local.sqlite3",
            "app/src/.bowline-materialize-main_rs-abcdef123456.tmp",
        ] {
            assert!(is_private_workspace_state_path(path), "{path}");
        }

        for path in [
            ".env",
            "apps/web/.env.local",
            ".git",
            ".git/HEAD",
            ".bowlineignore",
            ".bowline-conflicts/conflict/local/app.env",
            "project/.bowline/state.json",
            ".bowline-materialize-not-a-temp",
        ] {
            assert!(!is_private_workspace_state_path(path), "{path}");
        }
    }
}
