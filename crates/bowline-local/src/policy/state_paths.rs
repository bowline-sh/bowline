//! Workspace state-path predicates: which workspace-relative paths belong to
//! Bowline's private engine state, and which paths may carry secrets. Moved
//! here from the deleted old sync engine — `policy` is the single owner of
//! what syncs.

use super::is_project_env_name;

/// Prefix for uniquely owned workspace-volume directories used by transient
/// endpoint capability and clock probes. The full names use the existing
/// materialization-temp convention and are therefore derivable daemon state.
pub const ENDPOINT_PROBE_STATE_PREFIX: &str = ".bowline-materialize-endpoint-probe-";

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
        || path.split('/').any(|part| {
            (part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
                || is_recovery_temp_component(part)
                || is_recovery_quarantine_component(part)
        })
}

pub(crate) fn is_recovery_temp_component(name: &str) -> bool {
    is_recovery_nonce_component(name, ".bowline-recovery-")
}

pub(crate) fn is_recovery_quarantine_component(name: &str) -> bool {
    is_recovery_nonce_component(name, ".bowline-recovery-quarantine-")
}

fn is_recovery_nonce_component(name: &str, prefix: &str) -> bool {
    let Some(nonce) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
            ".bowline-materialize-endpoint-probe-123-1.tmp",
            ".bowline-materialize-endpoint-probe-123-1.tmp/endpoint-clock.probe",
            "app/src/.bowline-materialize-main_rs-abcdef123456.tmp",
            "vendor/.bowline-recovery-0123456789abcdef0123456789abcdef.tmp",
            "vendor/.bowline-recovery-quarantine-0123456789abcdef0123456789abcdef.tmp",
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
            "vendor/.bowline-recovery-notes.tmp",
            "vendor/.bowline-recovery-0123456789abcdef.tmp",
            "vendor/.bowline-recovery-0123456789ABCDEF0123456789ABCDEF.tmp",
        ] {
            assert!(!is_private_workspace_state_path(path), "{path}");
        }
    }
}
