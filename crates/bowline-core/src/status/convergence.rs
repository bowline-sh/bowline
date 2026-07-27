use crate::commands::StatusCommandOutput;

use super::{RepairCommand, reduce_status_facts};

pub const WORKSPACE_CONVERGENCE_FACT_ID: &str = "workspace-convergence";
pub const PROJECT_CONVERGENCE_FACT_ID: &str = "project-convergence";
/// The deletion breaker's own fact. Convergence-owned like the two above, so a
/// surface that replaces convergence replaces this with it rather than leaving a
/// refusal on screen that the live engine is no longer making.
pub const MASS_DELETION_BLOCKED_FACT_ID: &str = "sync-mass-deletion-blocked";

pub fn is_convergence_fact_id(id: &str) -> bool {
    matches!(
        id,
        WORKSPACE_CONVERGENCE_FACT_ID | PROJECT_CONVERGENCE_FACT_ID | MASS_DELETION_BLOCKED_FACT_ID
    )
}

/// The two-step loop a refused removal batch puts the user in: read exactly what
/// would be deleted, then authorise that one push.
///
/// Defined once because two surfaces publish it — the daemon's own projection
/// and the CLI status that overlays it — and a drift between them would point a
/// user at a command that does not exist.
fn mass_deletion_next_actions() -> [RepairCommand; 2] {
    [
        RepairCommand::inspect(
            "List the files this deletion would remove",
            Some("bowline deletions".to_string()),
        ),
        RepairCommand::mutating(
            "Confirm the deletion and let sync continue",
            Some("bowline deletions --confirm".to_string()),
        ),
    ]
}

/// Republish the next actions convergence owns, from the facts `output` now
/// carries. Idempotent by construction: [`remove_convergence_surfaces`] drops the
/// previous copy, and this is the only writer of them.
pub fn apply_convergence_next_actions(output: &mut StatusCommandOutput) {
    let blocked = output
        .status_summary
        .facts
        .iter()
        .any(|fact| fact.id.as_str() == MASS_DELETION_BLOCKED_FACT_ID);
    if blocked {
        output.next_actions.extend(mass_deletion_next_actions());
    }
}

pub fn remove_convergence_surfaces(output: &mut StatusCommandOutput) {
    let convergence_summaries = output
        .items
        .iter()
        .filter(|item| {
            item.subject
                .as_ref()
                .is_some_and(|subject| is_convergence_fact_id(&subject.id))
        })
        .map(|item| item.summary.clone())
        .collect::<Vec<_>>();
    output
        .status_summary
        .facts
        .retain(|fact| !is_convergence_fact_id(fact.id.as_str()));
    output.items.retain(|item| {
        !item
            .subject
            .as_ref()
            .is_some_and(|subject| is_convergence_fact_id(&subject.id))
    });
    output
        .limits
        .retain(|limit| !is_convergence_fact_id(&limit.capability));
    output
        .status
        .attention_items
        .retain(|summary| !convergence_summaries.contains(summary));
    let owned = mass_deletion_next_actions();
    output.next_actions.retain(|action| !owned.contains(action));
}

/// Merge the convergence-owned portion of a daemon projection into a
/// separately composed CLI status. Device trust, update, metadata, and other
/// local facts remain owned by the target; convergence has exactly one owner.
pub fn overlay_convergence_status(target: &mut StatusCommandOutput, source: &StatusCommandOutput) {
    remove_convergence_surfaces(target);
    target.convergence.clone_from(&source.convergence);
    target.sync_queue.clone_from(&source.sync_queue);

    target.status_summary.facts.extend(
        source
            .status_summary
            .facts
            .iter()
            .filter(|fact| is_convergence_fact_id(fact.id.as_str()))
            .cloned(),
    );
    let convergence_items = source
        .items
        .iter()
        .filter(|item| {
            item.subject
                .as_ref()
                .is_some_and(|subject| is_convergence_fact_id(&subject.id))
        })
        .cloned()
        .collect::<Vec<_>>();
    target.status.attention_items.extend(
        convergence_items
            .iter()
            .map(|item| item.summary.clone())
            .filter(|summary| source.status.attention_items.contains(summary)),
    );
    target.items.extend(convergence_items);
    target.limits.extend(
        source
            .limits
            .iter()
            .filter(|limit| is_convergence_fact_id(&limit.capability))
            .cloned(),
    );
    target.status.attention_items.sort();
    target.status.attention_items.dedup();

    let prior_freshness = target.status_summary.freshness;
    target.status_summary = reduce_status_facts(
        target.status_summary.facts.clone(),
        target.status_summary.snapshot_version,
        target.generated_at.clone(),
    );
    target.status_summary.freshness = prior_freshness;
    target.status.level = target.status_summary.presentation_level();
    apply_convergence_next_actions(target);
}
