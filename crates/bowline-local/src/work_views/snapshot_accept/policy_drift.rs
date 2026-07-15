use super::*;

pub(super) struct PolicyDriftInput<'a> {
    pub(super) store: &'a MetadataStore,
    pub(super) view: &'a bowline_core::work_views::WorkView,
    pub(super) workspace: &'a Path,
    pub(super) work_root: &'a Path,
    pub(super) main_root: &'a Path,
    pub(super) base: &'a Reader,
    pub(super) work: &'a Reader,
    pub(super) universe: &'a WorkCandidateUniverse,
}

pub(super) fn policy_drift_for_exposed(
    input: PolicyDriftInput<'_>,
    checkpoint: &mut dyn FnMut(LongOperationCancellationPoint) -> Result<(), WorkViewError>,
) -> Result<Vec<PolicyDriftRecord>, WorkViewError> {
    let work_by_path = input
        .work
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut drift = Vec::new();
    for base_entry in &input.base.entries {
        checkpoint(LongOperationCancellationPoint::BetweenChunks)?;
        let changed = work_by_path
            .get(base_entry.path.as_str())
            .is_none_or(|entry| entry.content_id != base_entry.content_id);
        if !changed {
            continue;
        }
        let workspace_path =
            workspace_path_for_project_file(input.view, Path::new(&base_entry.path));
        let work_path = input.work_root.join(&base_entry.path);
        let main_path = input.main_root.join(&base_entry.path);
        let source = if work_path.exists() {
            Some(work_path.as_path())
        } else if main_path.exists() {
            Some(main_path.as_path())
        } else {
            None
        };
        let policy = clean_accept_policy(
            input.store,
            input.workspace,
            &input.view.workspace_id,
            &workspace_path,
            source,
        )?;
        if let Some(reason) = input.universe.classify_drift(
            &base_entry.path,
            policy.classification,
            policy.mode,
            &policy.access,
            clean_accept_explicit_include(input.workspace, &workspace_path)?,
        ) {
            drift.push(PolicyDriftRecord {
                path: base_entry.path.clone(),
                reason,
            });
        }
    }
    Ok(drift)
}
