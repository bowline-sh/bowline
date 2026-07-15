use std::collections::BTreeMap;

#[cfg(test)]
use std::collections::BTreeSet;

use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::NamespaceEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewPolicyDrift {
    ExplicitIncludeRemoved,
    ExplicitIncludeAdded,
    NewlyHidden,
    NewlyBlocked,
    BecameMachineLocal,
    MaterializationModeChanged,
    ClassificationChanged,
}

impl WorkViewPolicyDrift {
    pub fn code(self) -> &'static str {
        match self {
            Self::ExplicitIncludeRemoved => "explicit-include-removed",
            Self::ExplicitIncludeAdded => "explicit-include-added",
            Self::NewlyHidden => "newly-hidden",
            Self::NewlyBlocked => "newly-blocked",
            Self::BecameMachineLocal => "became-machine-local",
            Self::MaterializationModeChanged => "materialization-mode-changed",
            Self::ClassificationChanged => "classification-changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDriftRecord {
    pub path: String,
    pub reason: WorkViewPolicyDrift,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCandidateUniverse {
    exposed: BTreeMap<String, NamespaceEntry>,
}

impl WorkCandidateUniverse {
    pub fn new(entries: impl IntoIterator<Item = NamespaceEntry>) -> Self {
        Self {
            exposed: entries
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn deletions<'a>(
        &'a self,
        observed_paths: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = &'a NamespaceEntry> {
        self.exposed
            .iter()
            .filter(|(path, entry)| {
                entry.kind == bowline_core::workspace_graph::NamespaceEntryKind::File
                    && !observed_paths.contains(*path)
            })
            .map(|(_, entry)| entry)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.exposed.contains_key(path)
    }

    pub fn classify_drift(
        &self,
        path: &str,
        current_classification: PathClassification,
        current_mode: MaterializationMode,
        current_access: &[AccessFlag],
        current_explicit_include: bool,
    ) -> Option<WorkViewPolicyDrift> {
        let base = self.exposed.get(path)?;
        if current_access.contains(&AccessFlag::AgentHidden)
            && !base.access.contains(&AccessFlag::AgentHidden)
        {
            return Some(WorkViewPolicyDrift::NewlyHidden);
        }
        if current_classification == PathClassification::Blocked
            || current_mode == MaterializationMode::Blocked
        {
            return Some(WorkViewPolicyDrift::NewlyBlocked);
        }
        if current_classification == PathClassification::LocalOnly
            || current_mode == MaterializationMode::LocalOnly
            || current_mode == MaterializationMode::Ignore
        {
            return Some(
                if current_mode == MaterializationMode::Ignore && !current_explicit_include {
                    WorkViewPolicyDrift::ExplicitIncludeRemoved
                } else {
                    WorkViewPolicyDrift::BecameMachineLocal
                },
            );
        }
        if current_classification != base.classification {
            return Some(WorkViewPolicyDrift::ClassificationChanged);
        }
        (current_mode != base.mode).then_some(WorkViewPolicyDrift::MaterializationModeChanged)
    }

    pub fn classify_new_path(
        &self,
        current_classification: PathClassification,
        current_mode: MaterializationMode,
        current_access: &[AccessFlag],
        current_explicit_include: bool,
    ) -> Option<WorkViewPolicyDrift> {
        if current_explicit_include {
            return Some(WorkViewPolicyDrift::ExplicitIncludeAdded);
        }
        if current_access.contains(&AccessFlag::AgentHidden) {
            return Some(WorkViewPolicyDrift::NewlyHidden);
        }
        if current_classification == PathClassification::Blocked
            || current_mode == MaterializationMode::Blocked
        {
            return Some(WorkViewPolicyDrift::NewlyBlocked);
        }
        if current_classification == PathClassification::LocalOnly
            || current_mode == MaterializationMode::LocalOnly
        {
            return Some(WorkViewPolicyDrift::BecameMachineLocal);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::workspace_graph::{FileExecutability, HydrationState, NamespaceEntryKind};

    use super::*;

    fn entry(path: &str) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::AgentReadable],
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: Some(1),
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }
    }

    #[test]
    fn deletion_universe_contains_only_exposed_paths() {
        let universe = WorkCandidateUniverse::new([entry("src/lib.rs")]);
        let observed = BTreeSet::from(["id_rsa".to_string()]);
        let deletions = universe
            .deletions(&observed)
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(deletions, vec!["src/lib.rs"]);
        assert!(!universe.contains("id_rsa"));
    }

    #[test]
    fn hidden_policy_change_is_typed() {
        let universe = WorkCandidateUniverse::new([entry("src/lib.rs")]);
        assert_eq!(
            universe.classify_drift(
                "src/lib.rs",
                PathClassification::WorkspaceSync,
                MaterializationMode::WorkspaceSync,
                &[AccessFlag::AgentHidden],
                false,
            ),
            Some(WorkViewPolicyDrift::NewlyHidden)
        );
    }

    #[test]
    fn explicit_include_changes_are_typed() {
        let universe = WorkCandidateUniverse::new([entry("generated/output.js")]);
        assert_eq!(
            universe.classify_drift(
                "generated/output.js",
                PathClassification::LocalOnly,
                MaterializationMode::Ignore,
                &[AccessFlag::HumanReadable],
                false,
            ),
            Some(WorkViewPolicyDrift::ExplicitIncludeRemoved)
        );
        assert_eq!(
            universe.classify_new_path(
                PathClassification::WorkspaceSync,
                MaterializationMode::WorkspaceSync,
                &[AccessFlag::AgentReadable],
                true,
            ),
            Some(WorkViewPolicyDrift::ExplicitIncludeAdded)
        );
    }
}
