use super::*;

pub(super) const COMMAND_REGISTRY: &[CommandSpec] = &[CommandSpec {
    group: "Support",
    name: "doctor",
    summary: "Run read-only engine diagnostics with redacted, fixed reason codes.",
    usage: "bowline doctor [--engine manifest] [--json]",
    options: &[ENGINE_OPTION, SOCKET_OPTION, GLOBAL_JSON_OPTION],
    positionals: NO_POSITIONALS,
    examples: &[ExampleSpec {
        command: "bowline doctor --engine manifest --json",
        summary: "Diagnose the manifest sync engine and print the redacted report.",
    }],
    json_output_type: "DoctorCommandOutput",
    side_effect_level: "read",
    supports_json: true,
    supports_dry_run: false,
    bounded_output: None,
    related_commands: &["status", "diagnostics collect"],
}];
