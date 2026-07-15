use std::{error::Error, fmt};

use crate::{
    ids::{ManifestDigest, ProjectId, SnapshotId, WorkspaceId},
    workspace_graph::{NamespaceEntry, SnapshotKind, WorkspaceRef, WorkspaceRelativePath},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMetadata {
    pub schema_version: u16,
    pub snapshot_id: SnapshotId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub kind: SnapshotKind,
    pub base_snapshot_id: Option<SnapshotId>,
    pub semantic_manifest_digest: ManifestDigest,
    pub entry_count: u64,
    pub refs: Vec<WorkspaceRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceOperationBudget {
    pub max_entries_visited: u64,
    pub max_diff_entries_visited: u64,
    pub max_mutations: u64,
    pub max_namespace_pages_loaded: u64,
    pub max_layout_records_loaded: u64,
    pub max_segment_pages_loaded: u64,
    pub max_metadata_bytes: u64,
}

impl NamespaceOperationBudget {
    pub const fn new(
        max_entries_visited: u64,
        max_diff_entries_visited: u64,
        max_mutations: u64,
    ) -> Self {
        Self {
            max_entries_visited,
            max_diff_entries_visited,
            max_mutations,
            max_namespace_pages_loaded: u64::MAX,
            max_layout_records_loaded: u64::MAX,
            max_segment_pages_loaded: u64::MAX,
            max_metadata_bytes: u64::MAX,
        }
    }

    pub const fn with_metadata_limits(
        mut self,
        max_namespace_pages_loaded: u64,
        max_layout_records_loaded: u64,
        max_segment_pages_loaded: u64,
        max_metadata_bytes: u64,
    ) -> Self {
        self.max_namespace_pages_loaded = max_namespace_pages_loaded;
        self.max_layout_records_loaded = max_layout_records_loaded;
        self.max_segment_pages_loaded = max_segment_pages_loaded;
        self.max_metadata_bytes = max_metadata_bytes;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NamespaceOperationCounters {
    pub entries_visited: u64,
    pub diff_entries_visited: u64,
    pub mutations_applied: u64,
    pub namespace_pages_loaded: u64,
    pub layout_records_loaded: u64,
    pub segment_pages_loaded: u64,
    pub metadata_bytes: u64,
    pub cancellation_checks: u64,
}

pub trait NamespaceCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug)]
struct NeverCancelled;

impl NamespaceCancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

static NEVER_CANCELLED: NeverCancelled = NeverCancelled;

pub struct NamespaceOperationContext<'a> {
    budget: NamespaceOperationBudget,
    counters: NamespaceOperationCounters,
    cancellation: &'a dyn NamespaceCancellation,
}

impl<'a> NamespaceOperationContext<'a> {
    pub fn new(
        budget: NamespaceOperationBudget,
        cancellation: &'a dyn NamespaceCancellation,
    ) -> Self {
        Self {
            budget,
            counters: NamespaceOperationCounters::default(),
            cancellation,
        }
    }

    pub fn uncancelled(budget: NamespaceOperationBudget) -> Self {
        Self::new(budget, &NEVER_CANCELLED)
    }

    pub fn counters(&self) -> NamespaceOperationCounters {
        self.counters
    }

    pub fn charge_entries(&mut self, count: u64) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.entries_visited = charge(
            NamespaceResource::EntriesVisited,
            self.counters.entries_visited,
            count,
            self.budget.max_entries_visited,
        )?;
        Ok(())
    }

    pub fn charge_diff_entries(&mut self, count: u64) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.diff_entries_visited = charge(
            NamespaceResource::DiffEntriesVisited,
            self.counters.diff_entries_visited,
            count,
            self.budget.max_diff_entries_visited,
        )?;
        Ok(())
    }

    pub fn charge_mutation(&mut self) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.mutations_applied = charge(
            NamespaceResource::MutationsApplied,
            self.counters.mutations_applied,
            1,
            self.budget.max_mutations,
        )?;
        Ok(())
    }

    pub fn charge_namespace_page(&mut self, encoded_bytes: u64) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.namespace_pages_loaded = charge(
            NamespaceResource::NamespacePagesLoaded,
            self.counters.namespace_pages_loaded,
            1,
            self.budget.max_namespace_pages_loaded,
        )?;
        self.charge_metadata_bytes(encoded_bytes)
    }

    pub fn ensure_namespace_page_capacity(
        &mut self,
        maximum_encoded_bytes: u64,
    ) -> Result<(), NamespaceReadError> {
        self.ensure_record_capacity(
            NamespaceResource::NamespacePagesLoaded,
            self.counters.namespace_pages_loaded,
            self.budget.max_namespace_pages_loaded,
            maximum_encoded_bytes,
        )
    }

    pub fn charge_layout_record(&mut self, encoded_bytes: u64) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.layout_records_loaded = charge(
            NamespaceResource::LayoutRecordsLoaded,
            self.counters.layout_records_loaded,
            1,
            self.budget.max_layout_records_loaded,
        )?;
        self.charge_metadata_bytes(encoded_bytes)
    }

    pub fn ensure_layout_record_capacity(
        &mut self,
        maximum_encoded_bytes: u64,
    ) -> Result<(), NamespaceReadError> {
        self.ensure_record_capacity(
            NamespaceResource::LayoutRecordsLoaded,
            self.counters.layout_records_loaded,
            self.budget.max_layout_records_loaded,
            maximum_encoded_bytes,
        )
    }

    pub fn charge_segment_page(&mut self, encoded_bytes: u64) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        self.counters.segment_pages_loaded = charge(
            NamespaceResource::SegmentPagesLoaded,
            self.counters.segment_pages_loaded,
            1,
            self.budget.max_segment_pages_loaded,
        )?;
        self.charge_metadata_bytes(encoded_bytes)
    }

    pub fn ensure_segment_page_capacity(
        &mut self,
        maximum_encoded_bytes: u64,
    ) -> Result<(), NamespaceReadError> {
        self.ensure_record_capacity(
            NamespaceResource::SegmentPagesLoaded,
            self.counters.segment_pages_loaded,
            self.budget.max_segment_pages_loaded,
            maximum_encoded_bytes,
        )
    }

    fn ensure_record_capacity(
        &mut self,
        resource: NamespaceResource,
        current_records: u64,
        maximum_records: u64,
        maximum_encoded_bytes: u64,
    ) -> Result<(), NamespaceReadError> {
        self.ensure_active()?;
        charge(resource, current_records, 1, maximum_records)?;
        charge(
            NamespaceResource::MetadataBytes,
            self.counters.metadata_bytes,
            maximum_encoded_bytes,
            self.budget.max_metadata_bytes,
        )?;
        Ok(())
    }

    fn charge_metadata_bytes(&mut self, encoded_bytes: u64) -> Result<(), NamespaceReadError> {
        self.counters.metadata_bytes = charge(
            NamespaceResource::MetadataBytes,
            self.counters.metadata_bytes,
            encoded_bytes,
            self.budget.max_metadata_bytes,
        )?;
        Ok(())
    }

    pub fn ensure_active(&mut self) -> Result<(), NamespaceReadError> {
        self.counters.cancellation_checks = self.counters.cancellation_checks.saturating_add(1);
        if self.cancellation.is_cancelled() {
            Err(NamespaceReadError::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn charge(
    resource: NamespaceResource,
    current: u64,
    count: u64,
    limit: u64,
) -> Result<u64, NamespaceReadError> {
    let observed = current
        .checked_add(count)
        .ok_or(NamespaceReadError::BudgetExceeded {
            resource,
            observed: u64::MAX,
            limit,
        })?;
    if observed > limit {
        return Err(NamespaceReadError::BudgetExceeded {
            resource,
            observed,
            limit,
        });
    }
    Ok(observed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceResource {
    EntriesVisited,
    DiffEntriesVisited,
    MutationsApplied,
    NamespacePagesLoaded,
    LayoutRecordsLoaded,
    SegmentPagesLoaded,
    MetadataBytes,
}

impl NamespaceResource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EntriesVisited => "entries-visited",
            Self::DiffEntriesVisited => "diff-entries-visited",
            Self::MutationsApplied => "mutations-applied",
            Self::NamespacePagesLoaded => "namespace-pages-loaded",
            Self::LayoutRecordsLoaded => "layout-records-loaded",
            Self::SegmentPagesLoaded => "segment-pages-loaded",
            Self::MetadataBytes => "metadata-bytes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceReadError {
    BudgetExceeded {
        resource: NamespaceResource,
        observed: u64,
        limit: u64,
    },
    Cancelled,
    InvalidPath {
        field: &'static str,
        reason: &'static str,
    },
    DuplicatePath {
        field: &'static str,
    },
    NonCanonicalOrder {
        field: &'static str,
    },
    UnsupportedFormat {
        record: &'static str,
        version: u16,
    },
    OversizedRecord {
        record: &'static str,
        encoded_bytes: u64,
        maximum_bytes: u64,
    },
    MissingRecord {
        record: &'static str,
    },
    CorruptGraph {
        reason: &'static str,
    },
}

impl fmt::Display for NamespaceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BudgetExceeded {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "namespace budget exceeded for {}: observed {observed}, limit {limit}",
                resource.as_str()
            ),
            Self::Cancelled => formatter.write_str("namespace operation was cancelled"),
            Self::InvalidPath { field, reason } => {
                write!(formatter, "invalid namespace {field}: {reason}")
            }
            Self::DuplicatePath { field } => {
                write!(formatter, "namespace contains a duplicate {field}")
            }
            Self::NonCanonicalOrder { field } => {
                write!(
                    formatter,
                    "namespace {field} values are not in canonical order"
                )
            }
            Self::UnsupportedFormat { record, version } => {
                write!(formatter, "unsupported {record} format version {version}")
            }
            Self::OversizedRecord {
                record,
                encoded_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "{record} exceeds its encoded-byte limit: {encoded_bytes} > {maximum_bytes}"
            ),
            Self::MissingRecord { record } => write!(formatter, "missing {record}"),
            Self::CorruptGraph { reason } => {
                write!(formatter, "corrupt namespace graph: {reason}")
            }
        }
    }
}

impl Error for NamespaceReadError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceScope {
    All,
    Prefix(WorkspaceRelativePath),
}

impl NamespaceScope {
    pub fn contains(&self, path: &WorkspaceRelativePath) -> bool {
        match self {
            Self::All => true,
            Self::Prefix(prefix) => path.is_equal_to_or_below(prefix),
        }
    }

    pub fn prefix(&self) -> WorkspaceRelativePath {
        match self {
            Self::All => WorkspaceRelativePath::new(""),
            Self::Prefix(prefix) => prefix.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceVisitControl {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisitOutcome {
    pub entries_visited: u64,
    pub stopped_early: bool,
}

pub trait EntryVisitor {
    fn visit(
        &mut self,
        entry: &NamespaceEntry,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<NamespaceVisitControl, NamespaceReadError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceDiff {
    Added(NamespaceEntry),
    Removed(NamespaceEntry),
    Modified {
        before: NamespaceEntry,
        after: NamespaceEntry,
    },
}

pub trait NamespaceDiffVisitor {
    fn visit(&mut self, difference: NamespaceDiff) -> Result<(), NamespaceReadError>;
}

pub trait NamespaceSnapshotReader {
    fn metadata(&self) -> &SnapshotMetadata;

    fn get(
        &self,
        path: &WorkspaceRelativePath,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<NamespaceEntry>, NamespaceReadError>;

    fn visit_prefix(
        &self,
        prefix: &WorkspaceRelativePath,
        visitor: &mut dyn EntryVisitor,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<VisitOutcome, NamespaceReadError>;

    fn diff(
        &self,
        other: &dyn NamespaceSnapshotReader,
        scope: &NamespaceScope,
        visitor: &mut dyn NamespaceDiffVisitor,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<VisitOutcome, NamespaceReadError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceMutation {
    Upsert(NamespaceEntry),
    Remove(WorkspaceRelativePath),
    RemovePrefix(WorkspaceRelativePath),
}

pub trait NamespaceSnapshotBuilder {
    type Output;

    fn apply(
        &mut self,
        mutation: NamespaceMutation,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceBuildError>;

    fn finish(
        self,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Self::Output, NamespaceBuildError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceBuildError {
    Read(NamespaceReadError),
}

impl fmt::Display for NamespaceBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(formatter),
        }
    }
}

impl Error for NamespaceBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
        }
    }
}

impl From<NamespaceReadError> for NamespaceBuildError {
    fn from(error: NamespaceReadError) -> Self {
        Self::Read(error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct FlagCancellation(AtomicBool);

    impl NamespaceCancellation for FlagCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn prefix_scope_is_component_aware_and_root_contains_every_path() {
        let src = WorkspaceRelativePath::new("src");
        let child = WorkspaceRelativePath::new("src/lib.rs");
        let collision = WorkspaceRelativePath::new("src-old/lib.rs");
        let root = WorkspaceRelativePath::new("");

        assert!(child.is_equal_to_or_below(&src));
        assert!(!collision.is_equal_to_or_below(&src));
        assert!(child.is_equal_to_or_below(&root));
        assert!(NamespaceScope::Prefix(src).contains(&child));
    }

    #[test]
    fn budget_errors_name_resource_observed_and_limit() {
        let mut context =
            NamespaceOperationContext::uncancelled(NamespaceOperationBudget::new(1, 1, 1));
        context.charge_entries(1).expect("first entry is in budget");

        assert_eq!(
            context.charge_entries(1),
            Err(NamespaceReadError::BudgetExceeded {
                resource: NamespaceResource::EntriesVisited,
                observed: 2,
                limit: 1,
            })
        );
    }

    #[test]
    fn cancellation_is_checked_before_budget_consumption() {
        let cancellation = FlagCancellation(AtomicBool::new(true));
        let mut context = NamespaceOperationContext::new(
            NamespaceOperationBudget::new(10, 10, 10),
            &cancellation,
        );

        assert_eq!(
            context.charge_entries(1),
            Err(NamespaceReadError::Cancelled)
        );
        assert_eq!(context.counters().entries_visited, 0);
        assert_eq!(context.counters().cancellation_checks, 1);
    }

    #[test]
    fn metadata_page_and_byte_budgets_are_independent_and_typed() {
        let budget = NamespaceOperationBudget::new(0, 0, 0).with_metadata_limits(1, 1, 1, 8);
        let mut context = NamespaceOperationContext::uncancelled(budget);
        context
            .charge_namespace_page(8)
            .expect("first namespace page fits both limits");

        assert_eq!(
            context.charge_namespace_page(1),
            Err(NamespaceReadError::BudgetExceeded {
                resource: NamespaceResource::NamespacePagesLoaded,
                observed: 2,
                limit: 1,
            })
        );
        assert_eq!(context.counters().namespace_pages_loaded, 1);
        assert_eq!(context.counters().metadata_bytes, 8);
    }
}
