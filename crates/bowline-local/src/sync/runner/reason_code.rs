//! Fixed, machine-readable reason vocabulary for externally-visible sync
//! surfaces (checkpoint payloads, daemon status JSON, hosted-readable
//! telemetry).
//!
//! Security invariant (Plan 06 Security/privacy contract): these surfaces are
//! aggregate-only. They may carry bounded step/state/mode/reason enums,
//! snapshot IDs, operation IDs, aggregate counts, and cost counters — never
//! workspace-relative or absolute paths, dirty-root names, symlink targets,
//! raw scanner errors, or `error.to_string()` output. Every reason that
//! crosses a checkpoint/status boundary is one of these codes, serialized at
//! the edge via [`as_code`](SyncExternalFailureCode::as_code); a raw error
//! string must never take its place.

/// Coarse, path-free classification of a sync failure surfaced through daemon
/// status JSON and hosted-readable telemetry.
///
/// The classification is deliberately coarse: it exists to tell a device or
/// support surface *what kind* of failure occurred without leaking the
/// underlying error text (which can embed workspace paths, dirty-root names,
/// or scanner internals). Some variants (`WatcherOverflow`, `WatcherUnavailable`,
/// `HeadManifestUnavailable`, `UnboundDeepFileEntry`) are produced by
/// daemon-side watcher/scan-scope classification rather than by
/// [`SyncRunnerError`](super::SyncRunnerError); they are part of the stable
/// vocabulary and get wired to their producers in later Plan 06 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncExternalFailureCode {
    HeadManifestUnavailable,
    UnboundDeepFileEntry,
    WatcherOverflow,
    WatcherUnavailable,
    StatCacheDivergence,
    MaterializationBlocked,
    ScannerReadFailed,
    ControlPlaneUnavailable,
    ImportedSnapshotInvalid,
    /// The device has no usable workspace key, so sync cannot decrypt/encrypt
    /// state. Surfaced (not redacted to `Unknown`) so status can tell the user
    /// sync is blocked on trust/auth — the message names no path or secret.
    WorkspaceKeyMissing,
    /// The device's workspace key is present but rejected (e.g. rotated out).
    WorkspaceKeyInvalid,
    Unknown,
}

impl SyncExternalFailureCode {
    /// Stable machine code emitted into external payloads. Must stay
    /// path-free and stable across releases; clients map it to display text.
    pub fn as_code(self) -> &'static str {
        match self {
            Self::HeadManifestUnavailable => "head-manifest-unavailable",
            Self::UnboundDeepFileEntry => "unbound-deep-file-entry",
            Self::WatcherOverflow => "watcher-overflow",
            Self::WatcherUnavailable => "watcher-unavailable",
            Self::StatCacheDivergence => "stat-cache-divergence",
            Self::MaterializationBlocked => "materialization-blocked",
            Self::ScannerReadFailed => "scanner-read-failed",
            Self::ControlPlaneUnavailable => "control-plane-unavailable",
            Self::ImportedSnapshotInvalid => "imported-snapshot-invalid",
            Self::WorkspaceKeyMissing => "workspace-key-missing",
            Self::WorkspaceKeyInvalid => "workspace-key-invalid",
            Self::Unknown => "unknown",
        }
    }
}

/// Fixed reason code for a sync checkpoint payload. Replaces the previous
/// free-text `reason` / `error.to_string()` values so a checkpoint can never
/// carry a path-bearing error string.
///
/// Only reasons with a live emission site exist here; later Plan 06 units
/// extend the vocabulary when they add their surfaces (e.g. U7 adds a
/// stat-cache projection over-budget reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointReasonCode {
    HeadManifestUnavailable,
    UnboundDeepFileEntry,
    SourcePackReuseUnavailable,
    RemoteImportBlocked,
    MergePluginConfigInvalid,
}

impl CheckpointReasonCode {
    pub(crate) fn as_code(self) -> &'static str {
        match self {
            Self::HeadManifestUnavailable => "head-manifest-unavailable",
            Self::UnboundDeepFileEntry => "unbound-deep-file-entry",
            Self::SourcePackReuseUnavailable => "source-pack-reuse-unavailable",
            Self::RemoteImportBlocked => "remote-import-blocked",
            Self::MergePluginConfigInvalid => "merge-plugin-config-invalid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::helpers::{SnapshotReasonPayload, checkpoint_payload};
    use super::*;
    use crate::sync::runner::SyncRunnerError;
    use bowline_core::ids::ContentId;

    // Canary paths from the Plan 06 Security/privacy contract. No external
    // payload derived from an error may contain any of these substrings.
    const CANARY_PATHS: &[&str] = &[".env", "secrets/prod.key", "client/acme-payroll/keys.json"];

    fn assert_pathless(value: &str) {
        for path in CANARY_PATHS {
            assert!(
                !value.contains(path),
                "external reason `{value}` leaked path substring `{path}`"
            );
        }
    }

    #[test]
    fn external_failure_codes_are_fixed_and_pathless() {
        for code in [
            SyncExternalFailureCode::HeadManifestUnavailable,
            SyncExternalFailureCode::UnboundDeepFileEntry,
            SyncExternalFailureCode::WatcherOverflow,
            SyncExternalFailureCode::WatcherUnavailable,
            SyncExternalFailureCode::StatCacheDivergence,
            SyncExternalFailureCode::MaterializationBlocked,
            SyncExternalFailureCode::ScannerReadFailed,
            SyncExternalFailureCode::ControlPlaneUnavailable,
            SyncExternalFailureCode::ImportedSnapshotInvalid,
            SyncExternalFailureCode::Unknown,
        ] {
            let text = code.as_code();
            assert!(!text.is_empty());
            assert!(text.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
            assert_pathless(text);
        }
    }

    #[test]
    fn checkpoint_reason_codes_are_fixed_and_pathless() {
        for code in [
            CheckpointReasonCode::HeadManifestUnavailable,
            CheckpointReasonCode::UnboundDeepFileEntry,
            CheckpointReasonCode::SourcePackReuseUnavailable,
            CheckpointReasonCode::RemoteImportBlocked,
            CheckpointReasonCode::MergePluginConfigInvalid,
        ] {
            let text = code.as_code();
            assert!(!text.is_empty());
            assert!(text.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
            assert_pathless(text);
        }
    }

    #[test]
    fn path_bearing_runner_errors_classify_to_pathless_codes() {
        let cases = [
            (
                SyncRunnerError::MaterializationBlockedByDirectory(
                    "client/acme-payroll/keys.json".to_string(),
                ),
                SyncExternalFailureCode::MaterializationBlocked,
            ),
            (
                SyncRunnerError::UnsafeMaterializationPath(".env".to_string()),
                SyncExternalFailureCode::MaterializationBlocked,
            ),
            (
                SyncRunnerError::StatCacheDivergence {
                    path: "secrets/prod.key".to_string(),
                    cached_content_id: ContentId::new("cid_cached".to_string()),
                    observed_content_id: ContentId::new("cid_observed".to_string()),
                },
                SyncExternalFailureCode::StatCacheDivergence,
            ),
        ];
        for (error, expected) in cases {
            // The raw Display MUST carry the path (that is why it cannot ship
            // externally); the classified code MUST NOT.
            let raw = error.to_string();
            assert!(CANARY_PATHS.iter().any(|path| raw.contains(path)));
            let code = error.external_failure_code();
            assert_eq!(code, expected);
            assert_pathless(code.as_code());
        }
    }

    #[test]
    fn checkpoint_payload_carries_code_not_error_text() {
        // The remote-import-blocked checkpoint previously serialized
        // `reason: error.to_string()`; prove the migrated payload carries the
        // fixed code and none of the canary paths.
        let payload = checkpoint_payload(&SnapshotReasonPayload {
            snapshot_id: "snap_abc123",
            reason: CheckpointReasonCode::RemoteImportBlocked.as_code(),
        })
        .expect("payload serializes");
        assert!(payload.contains("remote-import-blocked"));
        assert!(payload.contains("snap_abc123"));
        assert_pathless(&payload);
    }
}
