//! Internal, machine-facing command specs kept out of the public help/contract
//! surface. These commands have no `CommandName` wire entry; they dispatch by
//! `DefinitionTarget` and are discovered by tooling, not documented for humans.
use super::*;

const SYNC_WORKSPACE_OPTION: OptionSpec = OptionSpec {
    name: "--workspace",
    value_name: Some("id"),
    summary: "Workspace id to wait on.",
    required: true,
    repeatable: false,
};
const SYNC_STATE_OPTION: OptionSpec = OptionSpec {
    name: "--state",
    value_name: Some("state"),
    summary: "Semantic readiness rung to wait for: unauthenticated, approval-pending, authenticated, or ready.",
    required: true,
    repeatable: false,
};
const SYNC_TIMEOUT_OPTION: OptionSpec = OptionSpec {
    name: "--timeout",
    value_name: Some("duration"),
    summary: "Maximum time to wait, e.g. 120s or 2m (default 120s).",
    required: false,
    repeatable: false,
};
const SYNC_WAIT_OPTIONS: &[OptionSpec] = &[
    SYNC_WORKSPACE_OPTION,
    SYNC_STATE_OPTION,
    SYNC_TIMEOUT_OPTION,
    SOCKET_OPTION,
    GLOBAL_JSON_OPTION,
];

const SYNC_OPERATION_OPTION: OptionSpec = OptionSpec {
    name: "--operation",
    value_name: Some("id"),
    summary: "Select one parked sync operation by id.",
    required: false,
    repeatable: false,
};
const SYNC_ALL_OPTION: OptionSpec = OptionSpec {
    name: "--all",
    value_name: None,
    summary: "Act on every parked sync operation.",
    required: false,
    repeatable: false,
};
const SYNC_ATTENTION_OPTIONS: &[OptionSpec] = &[SOCKET_OPTION, GLOBAL_JSON_OPTION];
const SYNC_RETRY_OPTIONS: &[OptionSpec] = &[
    SYNC_OPERATION_OPTION,
    SYNC_ALL_OPTION,
    SOCKET_OPTION,
    GLOBAL_JSON_OPTION,
];
const SYNC_DISMISS_OPTIONS: &[OptionSpec] =
    &[SYNC_OPERATION_OPTION, SOCKET_OPTION, GLOBAL_JSON_OPTION];

pub(super) const INTERNAL_COMMAND_REGISTRY: &[CommandSpec] = &[
    CommandSpec {
        group: "Internal",
        name: "sync attention",
        summary: "List sync operations parked in the attention lane.",
        usage: "bowline sync attention [--json]",
        options: SYNC_ATTENTION_OPTIONS,
        positionals: NO_POSITIONALS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "SyncAttentionCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
    CommandSpec {
        group: "Internal",
        name: "sync retry",
        summary: "Re-queue a parked sync operation with a fresh retry budget.",
        usage: "bowline sync retry --operation <id> | --all [--json]",
        options: SYNC_RETRY_OPTIONS,
        positionals: NO_POSITIONALS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "SyncRetryCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
    CommandSpec {
        group: "Internal",
        name: "sync dismiss",
        summary: "Dismiss a genuinely stuck sync operation with an audit event.",
        usage: "bowline sync dismiss --operation <id> [--json]",
        options: SYNC_DISMISS_OPTIONS,
        positionals: NO_POSITIONALS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "SyncDismissCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
    CommandSpec {
        group: "Internal",
        name: "sync wait",
        summary: "Block until the daemon reports the workspace at or past a readiness state.",
        usage: "bowline sync wait --workspace <id> --state <state> [--timeout 120s] [--json]",
        options: SYNC_WAIT_OPTIONS,
        positionals: NO_POSITIONALS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "SyncWaitCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
    CommandSpec {
        group: "Internal",
        name: "debug classify",
        summary: "Classify one path without exposing file contents.",
        usage: "bowline debug classify <path> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        positionals: REQUIRED_TARGET_POSITIONAL,
        examples: EMPTY_EXAMPLES,
        json_output_type: "DebugClassifyCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
];
