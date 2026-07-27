use super::*;
use crate::cli::parse_args;
use bowline_core::commands::BlockedDeletionBatch;

fn batch(removals: u64, listed: u64) -> BlockedDeletionBatch {
    BlockedDeletionBatch {
        removals,
        entries: 121,
        threshold: 64,
        paths: (0..listed)
            .map(|index| format!("notes/f{index}.md"))
            .collect(),
        listed,
    }
}

#[test]
fn the_preview_is_read_only_and_chains_to_the_confirmation() {
    let output = preview_output(BlockedDeletionsReport {
        state: DeletionsState::Blocked,
        blocked: Some(batch(72, 72)),
    });

    assert_eq!(output.state, DeletionsState::Blocked);
    assert!(!output.changed, "a preview never mutates");
    assert_eq!(output.confirmation, None);
    assert_eq!(
        output
            .next_actions
            .iter()
            .map(|action| (action.command.clone(), action.mutates))
            .collect::<Vec<_>>(),
        vec![(Some("bowline deletions --confirm".to_string()), true)],
        "the preview points at the one command that clears the block"
    );
}

#[test]
fn a_clear_workspace_offers_no_action() {
    let output = preview_output(BlockedDeletionsReport {
        state: DeletionsState::Clear,
        blocked: None,
    });

    assert_eq!(output.state, DeletionsState::Clear);
    assert!(output.blocked.is_none());
    assert!(output.next_actions.is_empty());
    assert!(!output.changed);
}

#[test]
fn a_confirmation_reports_the_batch_it_released() {
    let output = confirmed_output(DeletionsConfirmationReport {
        state: DeletionsConfirmation::Authorized,
        blocked: Some(batch(72, 72)),
    });

    assert!(output.changed);
    assert_eq!(output.confirmation, Some(DeletionsConfirmation::Authorized));
    assert_eq!(output.blocked.map(|blocked| blocked.removals), Some(72));
}

#[test]
fn confirming_nothing_is_a_success_that_changed_nothing() {
    let output = confirmed_output(DeletionsConfirmationReport {
        state: DeletionsConfirmation::NotBlocked,
        blocked: None,
    });

    assert_eq!(output.state, DeletionsState::Clear);
    assert!(
        !output.changed,
        "a confirmation that authorised nothing must not claim it changed the workspace"
    );
    assert!(output.next_actions.is_empty());
}

#[test]
fn the_human_rendering_bounds_the_path_list_and_names_the_remainder() {
    let output = preview_output(BlockedDeletionsReport {
        state: DeletionsState::Blocked,
        blocked: Some(batch(500, 200)),
    });

    let rendered = render_deletions_human(&output, 20);

    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with("  notes/"))
            .count(),
        20,
        "a terminal gets a page, never the whole batch"
    );
    assert!(
        rendered.contains("and 480 more"),
        "the remainder is counted against the real batch, not the listed sample: {rendered}"
    );
    assert!(rendered.contains("bowline deletions --confirm"));
}

#[test]
fn the_json_contract_carries_the_command_and_state_tokens() {
    let output = DeletionsCommandOutput {
        generated_at: "2026-07-25T00:00:00Z".to_string(),
        ..preview_output(BlockedDeletionsReport {
            state: DeletionsState::Blocked,
            blocked: Some(batch(72, 72)),
        })
    };

    let value = serde_json::to_value(&output).expect("deletions output serializes");

    assert_eq!(value["command"], "deletions");
    assert_eq!(value["state"], "blocked");
    assert_eq!(value["changed"], false);
    assert_eq!(value["blocked"]["removals"], 72);
    assert_eq!(value["blocked"]["threshold"], 64);
    assert_eq!(value["nextActions"][0]["mutates"], true);
}

#[test]
fn the_registry_parses_both_modes_and_declares_the_mutating_one() {
    let preview = parse_args(["deletions", "--json"]);
    assert_eq!(
        preview.command,
        Ok(Command::Deletions(DeletionsArgs { confirm: false }))
    );
    let confirm = parse_args(["deletions", "--confirm", "--json"]);
    assert_eq!(
        confirm.command,
        Ok(Command::Deletions(DeletionsArgs { confirm: true }))
    );
    // Both modes share one spec, so the declared level must be the conditional
    // one a harness can gate on.
    assert_eq!(
        confirm.side_effect_level,
        Some(crate::registry::SideEffectLevel::ConditionalMutation)
    );
}
