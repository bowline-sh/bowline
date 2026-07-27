use super::*;

pub(super) const COMMAND_REGISTRY: &[CommandSpec] = &[CommandSpec {
    group: "Support",
    name: "diagnostics collect",
    summary: "Print a redacted diagnostics bundle.",
    usage: "bowline diagnostics collect [--root <path>] [--json]",
    options: &[OPTIONAL_ROOT_OPTION, SOCKET_OPTION, GLOBAL_JSON_OPTION],
    positionals: NO_POSITIONALS,
    examples: EMPTY_EXAMPLES,
    json_output_type: "DiagnosticsCollectCommandOutput",
    side_effect_level: SideEffectLevel::Read,
    bounded_output: None,
    related_commands: &["status"],
}];
