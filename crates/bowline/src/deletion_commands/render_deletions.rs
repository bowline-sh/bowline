//! Human rendering for `bowline deletions`.
//!
//! Two audiences, one text: a person who just deleted a directory on purpose and
//! wants to get on with it, and a person who has no idea why sync stopped. Both
//! need the same three things — that sync is paused, exactly what would go, and
//! the one command that resumes it.

use bowline_core::commands::{
    BlockedDeletionBatch, DeletionsCommandOutput, DeletionsConfirmation, DeletionsState,
};

pub(crate) fn render_deletions_human(output: &DeletionsCommandOutput, path_limit: usize) -> String {
    match (output.state, output.confirmation) {
        (DeletionsState::Clear, None) => {
            "No deletions are waiting for confirmation. Sync is publishing normally.\n".to_string()
        }
        (DeletionsState::Clear, Some(_)) => {
            "Nothing was waiting for confirmation, so nothing changed.\n".to_string()
        }
        (DeletionsState::Blocked, None) => render_preview(output, path_limit),
        (DeletionsState::Blocked, Some(DeletionsConfirmation::Authorized)) => {
            render_confirmed(output.blocked.as_ref())
        }
        // `blocked` with a `not-blocked` confirmation cannot be produced by the
        // daemon; report the honest fallback rather than inventing a story.
        (DeletionsState::Blocked, Some(DeletionsConfirmation::NotBlocked)) => {
            "Nothing was waiting for confirmation, so nothing changed.\n".to_string()
        }
    }
}

fn render_preview(output: &DeletionsCommandOutput, path_limit: usize) -> String {
    let Some(batch) = output.blocked.as_ref() else {
        return "Sync is paused on a deletion, but the daemon reported no detail.\n".to_string();
    };
    let mut text = format!(
        "Sync is paused. Publishing would delete {} of {} synced entries, above the {} allowed \
         without confirmation.\n\n",
        batch.removals, batch.entries, batch.threshold
    );
    text.push_str(&render_paths(batch, path_limit));
    text.push_str(
        "\nIf these deletions are what you intended, run:\n  bowline deletions --confirm\n\
         That authorises this one push. Otherwise restore the files and sync resumes on its own.\n",
    );
    text
}

fn render_paths(batch: &BlockedDeletionBatch, path_limit: usize) -> String {
    let mut text = String::new();
    for path in batch.paths.iter().take(path_limit) {
        text.push_str(&format!("  {path}\n"));
    }
    let shown = batch.paths.len().min(path_limit) as u64;
    if batch.removals > shown {
        text.push_str(&format!(
            "  ... and {} more (run `bowline deletions --json` for the full list)\n",
            batch.removals - shown
        ));
    }
    text
}

fn render_confirmed(batch: Option<&BlockedDeletionBatch>) -> String {
    let Some(batch) = batch else {
        return "Confirmed. Sync will publish the pending deletion on its next cycle.\n"
            .to_string();
    };
    format!(
        "Confirmed: {} deletions will publish on the next sync cycle. This authorised one push \
         only.\n",
        batch.removals
    )
}
