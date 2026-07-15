use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::{
    MergePluginIdentity,
    matcher::{MAX_GLOB_MATCH_BYTES, policy_bound_matcher_version},
};

const MATCHER_CONTRACT_VERSION: &str = "2";

#[derive(Debug, Deserialize)]
pub(super) struct ProjectMergePluginConfig {
    schema: Option<u32>,
    #[serde(default)]
    pub(super) plugins: Vec<ProjectMergePluginDeclaration>,
}

impl ProjectMergePluginConfig {
    pub(super) fn parse(text: &str) -> Result<Self, MergePluginConfigError> {
        let config: Self = toml::from_str(text)?;
        if config.schema.unwrap_or(1) != 1 {
            return Err(MergePluginConfigError::UnsupportedSchema(
                config.schema.unwrap_or_default(),
            ));
        }
        for plugin in &config.plugins {
            for pattern in &plugin.patterns {
                if pattern.len() > MAX_GLOB_MATCH_BYTES {
                    return Err(MergePluginConfigError::MatchPatternTooLong {
                        plugin_id: plugin.id.clone(),
                        bytes: pattern.len(),
                        max_bytes: MAX_GLOB_MATCH_BYTES,
                    });
                }
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct ProjectMergePluginDeclaration {
    id: String,
    version: String,
    digest: String,
    pub(super) module: String,
    #[serde(rename = "match")]
    pub(super) patterns: Vec<String>,
    #[serde(default = "default_matcher_contract_version")]
    matcher_version: String,
    #[serde(default = "default_validator_contract_version")]
    validator_version: String,
}

impl ProjectMergePluginDeclaration {
    pub(super) fn identity(&self) -> MergePluginIdentity {
        MergePluginIdentity {
            id: self.id.clone(),
            version: self.version.clone(),
            digest: self.digest.clone(),
            matcher_version: policy_bound_matcher_version(&self.matcher_version, &self.patterns),
            validator_version: self.validator_version.clone(),
        }
    }

    pub(super) fn unsupported_matcher_contract(&self) -> Option<&str> {
        (self.matcher_version != MATCHER_CONTRACT_VERSION).then_some(self.matcher_version.as_str())
    }
}

#[derive(Debug)]
pub enum MergePluginConfigError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    UnsupportedSchema(u32),
    UnsafeModulePath(String),
    MatchPatternTooLong {
        plugin_id: String,
        bytes: usize,
        max_bytes: usize,
    },
}

impl fmt::Display for MergePluginConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "merge plugin config I/O failed: {error}"),
            Self::Toml(error) => write!(formatter, "merge plugin config is invalid TOML: {error}"),
            Self::UnsupportedSchema(schema) => {
                write!(formatter, "unsupported merge plugin config schema {schema}")
            }
            Self::UnsafeModulePath(path) => {
                write!(formatter, "unsafe merge plugin module path `{path}`")
            }
            Self::MatchPatternTooLong {
                plugin_id,
                bytes,
                max_bytes,
            } => write!(
                formatter,
                "merge plugin `{plugin_id}` match pattern is {bytes} bytes, exceeding {max_bytes}"
            ),
        }
    }
}

impl Error for MergePluginConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Toml(error) => Some(error),
            Self::UnsupportedSchema(_)
            | Self::UnsafeModulePath(_)
            | Self::MatchPatternTooLong { .. } => None,
        }
    }
}

impl From<toml::de::Error> for MergePluginConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Toml(error)
    }
}

pub(super) fn validate_project_relative_module_path(
    path: &str,
) -> Result<PathBuf, MergePluginConfigError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(MergePluginConfigError::UnsafeModulePath(
            path.display().to_string(),
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(MergePluginConfigError::UnsafeModulePath(
            path.display().to_string(),
        ));
    }
    Ok(path.to_path_buf())
}

fn default_matcher_contract_version() -> String {
    MATCHER_CONTRACT_VERSION.to_string()
}

fn default_validator_contract_version() -> String {
    "1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_requires_safe_relative_module_paths() {
        assert!(validate_project_relative_module_path(".bowline/plugins/a.wasm").is_ok());
        assert!(validate_project_relative_module_path("../a.wasm").is_err());
        assert!(validate_project_relative_module_path("/tmp/a.wasm").is_err());
    }

    #[test]
    fn project_config_retains_stale_matcher_contract_declarations() {
        let config = ProjectMergePluginConfig::parse(
            r#"
schema = 1

[[plugins]]
id = "notebooks"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/notebooks.wasm"
match = ["*.ipynb"]
matcher-version = "1"
"#,
        )
        .expect("stale matcher contract remains a per-plugin decision");

        assert_eq!(config.plugins[0].unsupported_matcher_contract(), Some("1"));
    }

    #[test]
    fn example_configs_declare_only_the_supported_matcher_contract() {
        // Regression guard for the dead on-ramp: an unsupported matcher
        // contract never emits policy.needs_approval, so copied examples
        // silently do nothing even though the compile gate still passes.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/merge-plugins");
        for name in ["json-validator", "jupyter-notebook", "opaque-format"] {
            let text = std::fs::read_to_string(root.join(name).join(".bowlinemerge.toml"))
                .expect("example config present");
            let config = ProjectMergePluginConfig::parse(&text).expect("example config parses");
            for plugin in &config.plugins {
                assert_eq!(
                    plugin.unsupported_matcher_contract(),
                    None,
                    "example `{name}` declares an unsupported matcher contract",
                );
            }
        }
    }

    #[test]
    fn project_config_rejects_oversized_match_patterns() {
        let pattern = "*".repeat(MAX_GLOB_MATCH_BYTES + 1);
        let result = ProjectMergePluginConfig::parse(&format!(
            r#"
schema = 1

[[plugins]]
id = "hostile"
version = "1.0.0"
digest = "blake3:abcd"
module = ".bowline/plugins/hostile.wasm"
match = ["{pattern}"]
"#
        ));

        assert!(matches!(
            result,
            Err(MergePluginConfigError::MatchPatternTooLong {
                plugin_id,
                bytes,
                max_bytes,
            }) if plugin_id == "hostile"
                && bytes == MAX_GLOB_MATCH_BYTES + 1
                && max_bytes == MAX_GLOB_MATCH_BYTES
        ));
    }
}
