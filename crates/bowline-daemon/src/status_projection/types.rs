use std::{collections::BTreeMap, fmt, time::Duration};

use bowline_core::commands::StatusCommandOutput;
use serde::Serialize;

use super::collector::{StatusSourceFacts, StatusSourceStateFacts};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DaemonInstanceId(String);

impl DaemonInstanceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatusSequence(u64);

impl StatusSequence {
    pub const INITIAL: Self = Self(1);

    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatusSourceRevision(u64);

impl StatusSourceRevision {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatusTimestamp(String);

impl StatusTimestamp {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSource {
    Metadata,
    SyncRuntime,
    StoreHealth,
    DeviceTrust,
    UpdateAvailability,
    NotificationState,
    ServiceRuntime,
}

impl StatusSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::SyncRuntime => "sync-runtime",
            Self::StoreHealth => "store-health",
            Self::DeviceTrust => "device-trust",
            Self::UpdateAvailability => "update-availability",
            Self::NotificationState => "notification-state",
            Self::ServiceRuntime => "service-runtime",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceFreshness {
    Current,
    Stale,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRevision {
    pub source: StatusSource,
    pub revision: StatusSourceRevision,
    pub observed_at: StatusTimestamp,
    pub freshness: SourceFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFingerprint([u8; 32]);

impl StatusFingerprint {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for StatusFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionBuildReason {
    Initial,
    SourceChanged,
    RefreshAll,
    SourceFailure,
    Retry,
    SafetyRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatusProjection {
    pub instance_id: DaemonInstanceId,
    pub sequence: StatusSequence,
    pub semantic_fingerprint: StatusFingerprint,
    pub generated_at: StatusTimestamp,
    pub sources: BTreeMap<StatusSource, SourceRevision>,
    pub source_facts: BTreeMap<StatusSource, StatusSourceFacts>,
    pub status: StatusCommandOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionServiceConfig {
    instance_id: DaemonInstanceId,
    heartbeat_interval: Duration,
    safety_refresh_interval: SafetyRefreshInterval,
    retry_policy: StatusRetryPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetyRefreshInterval(Duration);

impl SafetyRefreshInterval {
    pub fn new(interval: Duration) -> Result<Self, StatusProjectionError> {
        if interval < Duration::from_millis(1) {
            return Err(StatusProjectionError::InvalidSafetyRefreshInterval);
        }
        Ok(Self(interval))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRetryPolicy {
    initial_delay: Duration,
    max_delay: Duration,
}

impl StatusRetryPolicy {
    pub fn new(
        initial_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, StatusProjectionError> {
        if initial_delay < Duration::from_millis(1) || max_delay < initial_delay {
            return Err(StatusProjectionError::InvalidRetryPolicy);
        }
        Ok(Self {
            initial_delay,
            max_delay,
        })
    }

    pub fn initial_delay(self) -> Duration {
        self.initial_delay
    }

    pub fn max_delay(self) -> Duration {
        self.max_delay
    }
}

impl ProjectionServiceConfig {
    pub fn new(
        instance_id: DaemonInstanceId,
        heartbeat_interval: Duration,
    ) -> Result<Self, StatusProjectionError> {
        if heartbeat_interval < Duration::from_millis(1) {
            return Err(StatusProjectionError::InvalidHeartbeatInterval);
        }
        Ok(Self {
            instance_id,
            heartbeat_interval,
            safety_refresh_interval: SafetyRefreshInterval(Duration::from_secs(60)),
            retry_policy: StatusRetryPolicy {
                initial_delay: Duration::from_millis(250),
                max_delay: Duration::from_secs(30),
            },
        })
    }

    pub fn with_safety_refresh_interval(mut self, interval: SafetyRefreshInterval) -> Self {
        self.safety_refresh_interval = interval;
        self
    }

    pub fn with_retry_policy(mut self, policy: StatusRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    pub(crate) fn instance_id(&self) -> &DaemonInstanceId {
        &self.instance_id
    }

    pub(crate) fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    pub(crate) fn safety_refresh_interval(&self) -> Duration {
        self.safety_refresh_interval.get()
    }

    pub(crate) fn retry_policy(&self) -> StatusRetryPolicy {
        self.retry_policy
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusInputEvent {
    SourceChanged(StatusSource),
    RefreshAll,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusProjectionMetrics {
    pub builds_by_reason: BTreeMap<ProjectionBuildReason, u64>,
    pub collector_calls: BTreeMap<StatusSource, u64>,
    pub collector_skips: BTreeMap<StatusSource, u64>,
    pub collector_failures: BTreeMap<StatusSource, u64>,
    pub semantic_changes: u64,
    pub no_op_refreshes: u64,
    pub broadcasts: u64,
    pub heartbeats_emitted: u64,
    pub input_events_received: u64,
    pub input_events_coalesced: u64,
    pub input_wakes_coalesced: u64,
    pub max_pending_input_sources: u64,
    pub projection_updates_delivered: u64,
    pub projection_updates_coalesced: u64,
    pub projection_subscribers_disconnected: u64,
    pub heartbeat_deliveries: u64,
    pub heartbeat_deliveries_coalesced: u64,
    pub heartbeat_subscribers_disconnected: u64,
    pub projection_subscribers_active: u64,
    pub heartbeat_subscribers_active: u64,
    pub collector_retries_scheduled: BTreeMap<StatusSource, u64>,
    pub collector_retry_attempts: BTreeMap<StatusSource, u64>,
    pub collector_retry_recoveries: BTreeMap<StatusSource, u64>,
    pub collector_retry_accelerations: BTreeMap<StatusSource, u64>,
    pub collector_retry_abandoned: BTreeMap<StatusSource, u64>,
    pub collector_retry_delays_capped: BTreeMap<StatusSource, u64>,
    pub collector_retry_delay_nanos: BTreeMap<StatusSource, u128>,
    pub collector_contract_retries_scheduled: BTreeMap<StatusSource, u64>,
    pub collector_contract_retry_attempts: BTreeMap<StatusSource, u64>,
    pub collector_contract_retry_recoveries: BTreeMap<StatusSource, u64>,
    pub collector_contract_retry_delays_capped: BTreeMap<StatusSource, u64>,
    pub active_collector_retries: u64,
    pub max_pending_collector_retries: u64,
    pub safety_refreshes: u64,
    pub build_latency_nanos: u128,
    pub rpc_serializations: u64,
    pub hosted_serializations: u64,
    pub hosted_publish_attempts: u64,
    pub hosted_publish_successes: u64,
    pub hosted_publish_failures: u64,
    pub notification_candidates: u64,
    pub notification_suppressed: u64,
    pub notification_sent: u64,
    pub notification_failures: u64,
    pub finder_snapshot_writes: u64,
    pub finder_snapshot_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusProjectionError {
    DuplicateCollector {
        source: StatusSource,
    },
    MissingMetadataCollector,
    MissingMetadataFacts,
    InitialCollection {
        source: StatusSource,
        code: super::collector::StatusCollectorFailureCode,
    },
    SourceContract {
        source: StatusSource,
    },
    SerializeFingerprint,
    TimestampFormatting,
    InvalidHeartbeatInterval,
    InvalidSafetyRefreshInterval,
    InvalidRetryPolicy,
    ChannelClosed {
        operation: &'static str,
    },
    WorkerPanicked,
}

impl fmt::Display for StatusProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateCollector { source } => {
                write!(formatter, "duplicate status collector for {source:?}")
            }
            Self::MissingMetadataCollector => formatter.write_str("metadata collector is required"),
            Self::MissingMetadataFacts => formatter.write_str("metadata facts are unavailable"),
            Self::InitialCollection { source, code } => {
                write!(formatter, "initial {source:?} collection failed: {code:?}")
            }
            Self::SourceContract { source } => {
                write!(
                    formatter,
                    "collector violated the retained facts contract for {source:?}"
                )
            }
            Self::SerializeFingerprint => {
                formatter.write_str("status fingerprint serialization failed")
            }
            Self::TimestampFormatting => formatter.write_str("status timestamp formatting failed"),
            Self::InvalidHeartbeatInterval => {
                formatter.write_str("status heartbeat interval must be at least one millisecond")
            }
            Self::InvalidSafetyRefreshInterval => formatter
                .write_str("status safety refresh interval must be at least one millisecond"),
            Self::InvalidRetryPolicy => formatter.write_str(
                "status retry delay must be at least one millisecond and not exceed its cap",
            ),
            Self::ChannelClosed { operation } => {
                write!(
                    formatter,
                    "status projection channel closed during {operation}"
                )
            }
            Self::WorkerPanicked => formatter.write_str("status projection worker panicked"),
        }
    }
}

impl std::error::Error for StatusProjectionError {}

#[derive(Serialize)]
struct FingerprintSource {
    source: StatusSource,
    freshness: SourceFreshness,
}

#[derive(Serialize)]
struct FingerprintMaterial<'a> {
    status: serde_json::Value,
    sources: Vec<FingerprintSource>,
    supplemental_facts: Vec<SupplementalFingerprintFacts<'a>>,
}

#[derive(Serialize)]
struct SupplementalFingerprintFacts<'a> {
    source: StatusSource,
    facts: &'a StatusSourceStateFacts,
}

const NON_SEMANTIC_TIMESTAMP: &str = "<projection-observation-time>";

#[derive(Serialize)]
#[serde(transparent)]
struct SemanticStatusView(StatusCommandOutput);

impl SemanticStatusView {
    /// Timestamp policy for the complete `StatusCommandOutput` graph:
    ///
    /// - `generatedAt`, `statusSummary.observedAt`, and
    ///   `statusSummary.facts[].observedAt` are projection observation metadata.
    /// - `setupReadiness.updatedAt`, `eventWatermarks.lastScanAt`, fact
    ///   `staleAfter`, and every fact-parameter map entry are public domain state.
    ///   `eventWatermarks.eventLagMs` is also semantic temporal state, while the
    ///   current `DeviceApprovalAffordance` contains no timestamp fields.
    /// - source `observedAt`, local-fact `observedAt`, projection `generatedAt`,
    ///   and adapter `publishedAt` live outside this status view and are omitted
    ///   by the typed fingerprint material that owns those boundaries.
    ///
    /// Starting from the complete typed output means new public fields remain
    /// semantic unless this exact constructor deliberately classifies them.
    fn new(status: &StatusCommandOutput) -> Self {
        let mut status = status.clone();
        status.generated_at = NON_SEMANTIC_TIMESTAMP.to_string();
        status.status_summary.observed_at = NON_SEMANTIC_TIMESTAMP.to_string();
        for fact in &mut status.status_summary.facts {
            fact.observed_at = NON_SEMANTIC_TIMESTAMP.to_string();
        }
        Self(status)
    }
}

pub fn semantic_fingerprint(
    status: &StatusCommandOutput,
    sources: &BTreeMap<StatusSource, SourceRevision>,
    facts: &BTreeMap<StatusSource, StatusSourceFacts>,
) -> Result<StatusFingerprint, StatusProjectionError> {
    let status_value = serde_json::to_value(SemanticStatusView::new(status))
        .map_err(|_| StatusProjectionError::SerializeFingerprint)?;
    let status_value = canonicalize_order(status_value);
    let fingerprint_sources = sources
        .values()
        .map(|source| FingerprintSource {
            source: source.source,
            freshness: source.freshness,
        })
        .collect();
    let supplemental_facts = facts
        .iter()
        .filter_map(|(source, facts)| {
            facts
                .state_facts()
                .map(|facts| SupplementalFingerprintFacts {
                    source: *source,
                    facts,
                })
        })
        .collect();
    let material = FingerprintMaterial {
        status: status_value,
        sources: fingerprint_sources,
        supplemental_facts,
    };
    let bytes =
        serde_json::to_vec(&material).map_err(|_| StatusProjectionError::SerializeFingerprint)?;
    Ok(StatusFingerprint(*blake3::hash(&bytes).as_bytes()))
}

fn canonicalize_order(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            let mut values = values
                .into_iter()
                .map(canonicalize_order)
                .collect::<Vec<_>>();
            values.sort_by_cached_key(|value| serde_json::to_vec(value).unwrap_or_default());
            serde_json::Value::Array(values)
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonicalize_order(value)))
                .collect(),
        ),
        scalar => scalar,
    }
}
