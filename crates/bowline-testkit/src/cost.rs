use std::{error::Error, fmt};

use bowline_local::sync::ScanStats;
use bowline_storage::ByteStoreMetrics;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostReport {
    pub byte_store: ByteStoreMetrics,
    pub scan: ScanStats,
    pub control_plane_upload_intents: u64,
    pub peak_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CostBudget {
    pub max_put_count: Option<u64>,
    pub max_full_read_count: Option<u64>,
    pub max_range_read_count: Option<u64>,
    pub max_head_count: Option<u64>,
    pub max_bytes_uploaded: Option<u64>,
    pub max_files_hashed: Option<u64>,
    pub max_control_plane_upload_intents: Option<u64>,
    pub max_peak_memory_bytes: Option<u64>,
}

impl CostBudget {
    pub fn assert_report(&self, report: &CostReport) -> Result<(), CostBudgetError> {
        check_metric("put_count", report.byte_store.put_count, self.max_put_count)?;
        check_metric(
            "full_read_count",
            report.byte_store.full_read_count,
            self.max_full_read_count,
        )?;
        check_metric(
            "range_read_count",
            report.byte_store.range_read_count,
            self.max_range_read_count,
        )?;
        check_metric(
            "head_count",
            report.byte_store.head_count,
            self.max_head_count,
        )?;
        check_metric(
            "bytes_uploaded",
            report.byte_store.bytes_uploaded,
            self.max_bytes_uploaded,
        )?;
        check_metric(
            "files_hashed",
            report.scan.files_hashed,
            self.max_files_hashed,
        )?;
        check_metric(
            "control_plane_upload_intents",
            report.control_plane_upload_intents,
            self.max_control_plane_upload_intents,
        )?;
        if let Some(peak_memory_bytes) = report.peak_memory_bytes {
            check_metric(
                "peak_memory_bytes",
                peak_memory_bytes,
                self.max_peak_memory_bytes,
            )?;
        }
        Ok(())
    }
}

fn check_metric(
    metric: &'static str,
    observed: u64,
    cap: Option<u64>,
) -> Result<(), CostBudgetError> {
    let Some(cap) = cap else {
        return Ok(());
    };
    if observed > cap {
        return Err(CostBudgetError {
            metric,
            observed,
            cap,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostBudgetError {
    pub metric: &'static str,
    pub observed: u64,
    pub cap: u64,
}

impl fmt::Display for CostBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cost budget exceeded for {}: observed {}, cap {}",
            self.metric, self.observed, self.cap
        )
    }
}

impl Error for CostBudgetError {}
