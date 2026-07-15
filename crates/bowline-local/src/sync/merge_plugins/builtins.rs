const YAML_VALIDATION_MAX_BYTES: usize = 16 * 1024 * 1024;

struct BuiltInMergePlugin {
    matches_file_name: fn(&str) -> bool,
    merge_mode: BuiltInMergeMode,
    validate: fn(&[u8]) -> bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltInMergeMode {
    LineThenValidate,
    AlwaysConflict,
}

const BUILT_IN_MERGE_PLUGINS: &[BuiltInMergePlugin] = &[
    BuiltInMergePlugin {
        matches_file_name: is_conflict_by_default_structured_file_name,
        merge_mode: BuiltInMergeMode::AlwaysConflict,
        validate: |_| false,
    },
    BuiltInMergePlugin {
        matches_file_name: |file_name| file_name.ends_with(".json"),
        merge_mode: BuiltInMergeMode::LineThenValidate,
        validate: |bytes| serde_json::from_slice::<serde_json::Value>(bytes).is_ok(),
    },
    BuiltInMergePlugin {
        matches_file_name: |file_name| file_name.ends_with(".toml"),
        merge_mode: BuiltInMergeMode::LineThenValidate,
        validate: toml_merge_output_is_valid,
    },
    BuiltInMergePlugin {
        matches_file_name: |file_name| file_name.ends_with(".yaml") || file_name.ends_with(".yml"),
        merge_mode: BuiltInMergeMode::LineThenValidate,
        validate: yaml_merge_output_is_valid,
    },
    BuiltInMergePlugin {
        matches_file_name: |file_name| file_name.ends_with(".xml"),
        merge_mode: BuiltInMergeMode::LineThenValidate,
        validate: xml_merge_output_is_valid,
    },
];

fn built_in_merge_plugin_for_path(path: &str) -> Option<&'static BuiltInMergePlugin> {
    let file_name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    BUILT_IN_MERGE_PLUGINS
        .iter()
        .find(|plugin| (plugin.matches_file_name)(&file_name))
}

pub(crate) fn built_in_merge_plugin_conflicts_by_default(path: &str) -> bool {
    built_in_merge_plugin_for_path(path)
        .is_some_and(|plugin| plugin.merge_mode == BuiltInMergeMode::AlwaysConflict)
}

pub(crate) fn structured_merge_output_is_valid(path: &str, bytes: &[u8]) -> bool {
    built_in_merge_plugin_for_path(path).is_none_or(|plugin| (plugin.validate)(bytes))
}

fn toml_merge_output_is_valid(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some()
}

fn yaml_merge_output_is_valid(bytes: &[u8]) -> bool {
    if bytes.len() > YAML_VALIDATION_MAX_BYTES {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 250_000,
            max_aliases: 1_000,
            max_anchors: 1_000,
            max_depth: 256,
            max_documents: 32,
            max_nodes: 100_000,
            max_total_scalar_bytes: YAML_VALIDATION_MAX_BYTES,
            max_merge_keys: 1_000,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_replay_stack_depth: 32,
            max_alias_expansions_per_anchor: 16,
        },
    };
    serde_saphyr::from_str_with_options::<serde::de::IgnoredAny>(text, options).is_ok()
}

fn xml_merge_output_is_valid(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| roxmltree::Document::parse(text).ok())
        .is_some()
}

fn is_conflict_by_default_structured_file_name(file_name: &str) -> bool {
    matches!(
        file_name,
        "cargo.lock" | "uv.lock" | "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_merge_output_validation_rejects_invalid_yaml_bytes() {
        assert!(structured_merge_output_is_valid(
            "settings.yaml",
            b"agent:\n  enabled: true\n",
        ));
        assert!(!structured_merge_output_is_valid(
            "settings.yaml",
            b"agent: [unterminated\n",
        ));
        assert!(!structured_merge_output_is_valid(
            "settings.yaml",
            b"\xff\xfe",
        ));
    }

    #[test]
    fn built_in_merge_plugin_registry_keeps_lockfiles_conflict_first() {
        assert!(built_in_merge_plugin_conflicts_by_default(
            "package-lock.json"
        ));
        assert!(!built_in_merge_plugin_conflicts_by_default("settings.json"));
        assert!(structured_merge_output_is_valid(
            "settings.json",
            br#"{"agent":true}"#,
        ));
        assert!(!structured_merge_output_is_valid(
            "settings.json",
            br#"{"agent":"#,
        ));
    }
}
