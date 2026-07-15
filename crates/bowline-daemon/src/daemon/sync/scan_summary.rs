use super::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::daemon) struct SyncScanSummary {
    pub(super) mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) full_reason: Option<&'static str>,
    pub(super) files_hashed: u64,
    pub(super) stat_hits: u64,
    pub(super) future_mtime_paths: u64,
    pub(super) divergence_count: u64,
    pub(super) rehash_reasons: Vec<SyncScanRehashSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SyncScanRehashSummary {
    pub(super) reason: &'static str,
    pub(super) count: u64,
}

impl Default for SyncScanSummary {
    fn default() -> Self {
        Self::from_scope_and_stats(&ScanScope::default(), &ScanStats::default())
    }
}

impl SyncScanSummary {
    pub(super) fn from_scope_and_stats(scope: &ScanScope, stats: &ScanStats) -> Self {
        let (mode, full_reason) = match scope {
            ScanScope::Full(reason) => (scan_mode_for_full_reason(*reason), Some(reason.as_str())),
            // `scoped+root` distinguishes the combined subtree+root-shallow pass
            // so status readers can tell it from a plain scoped scan.
            ScanScope::DirtySubtrees {
                root_shallow: true, ..
            } => ("scoped+root", None),
            ScanScope::DirtySubtrees {
                root_shallow: false,
                ..
            } => ("scoped", None),
            ScanScope::RootShallow => ("root-shallow", None),
        };
        Self {
            mode,
            full_reason,
            files_hashed: stats.files_hashed,
            stat_hits: stats.stat_hits,
            future_mtime_paths: stats.future_mtime_paths,
            divergence_count: stats.divergence_count,
            rehash_reasons: stats
                .rehash_reasons
                .iter()
                .map(|(reason, count)| SyncScanRehashSummary {
                    reason: reason.as_str(),
                    count: *count,
                })
                .collect(),
        }
    }
}

fn scan_mode_for_full_reason(reason: FullScanReason) -> &'static str {
    match reason {
        FullScanReason::VerifyDue => "verify",
        _ => "full",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // Canary paths / dirty-root names from the Plan 06 Security/privacy
    // contract. Scan-summary JSON is a hosted-readable status surface and must
    // stay aggregate-only, so none of these may ever appear in it.
    const CANARY_SUBSTRINGS: &[&str] = &[
        ".env",
        "secrets/prod.key",
        "client/acme-payroll/keys.json",
        "acme-payroll",
    ];

    fn assert_aggregate_only(summary: &SyncScanSummary) {
        let json = serde_json::to_string(summary).expect("summary serializes");
        for needle in CANARY_SUBSTRINGS {
            assert!(
                !json.contains(needle),
                "scan summary leaked sensitive substring `{needle}`: {json}"
            );
        }
    }

    #[test]
    fn full_scan_summary_is_aggregate_only() {
        let summary = SyncScanSummary::from_scope_and_stats(
            &ScanScope::Full(FullScanReason::CliRequested),
            &ScanStats::default(),
        );
        assert_eq!(summary.mode, "full");
        assert_aggregate_only(&summary);
    }

    #[test]
    fn scoped_scan_summary_never_serializes_dirty_root_names() {
        // Dirty-root names are forbidden on hosted surfaces; the scoped-scan
        // summary must report only the mode, never the roots it scanned.
        let mut roots = BTreeSet::new();
        roots.insert(".env".to_string());
        roots.insert("client/acme-payroll".to_string());
        roots.insert("secrets".to_string());
        let summary = SyncScanSummary::from_scope_and_stats(
            &ScanScope::DirtySubtrees {
                roots,
                root_shallow: false,
            },
            &ScanStats::default(),
        );
        assert_eq!(summary.mode, "scoped");
        assert_eq!(summary.full_reason, None);
        assert_aggregate_only(&summary);
    }

    #[test]
    fn root_shallow_scan_summary_maps_to_root_shallow_mode() {
        let summary =
            SyncScanSummary::from_scope_and_stats(&ScanScope::RootShallow, &ScanStats::default());
        assert_eq!(summary.mode, "root-shallow");
        assert_eq!(summary.full_reason, None);
        assert_aggregate_only(&summary);
    }

    #[test]
    fn combined_scan_summary_maps_to_scoped_root_mode_without_leaking_roots() {
        // The combined subtree+root-shallow pass reports `scoped+root` and, like
        // the plain scoped summary, must never serialize the roots it scanned.
        let mut roots = BTreeSet::new();
        roots.insert(".env".to_string());
        roots.insert("client/acme-payroll".to_string());
        roots.insert("secrets".to_string());
        let summary = SyncScanSummary::from_scope_and_stats(
            &ScanScope::DirtySubtrees {
                roots,
                root_shallow: true,
            },
            &ScanStats::default(),
        );
        assert_eq!(summary.mode, "scoped+root");
        assert_eq!(summary.full_reason, None);
        assert_aggregate_only(&summary);
    }
}
