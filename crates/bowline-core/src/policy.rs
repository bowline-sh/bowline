use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathClassification {
    WorkspaceSync,
    ProjectEnv,
    Generated,
    Dependency,
    Cache,
    LargeFile,
    SecretLooking,
    LocalOnly,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationMode {
    WorkspaceSync,
    ProjectEnv,
    EncryptedSync,
    Lazy,
    StructureOnly,
    LocalRegenerate,
    LocalCache,
    Ignore,
    LocalOnly,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessFlag {
    HumanReadable,
    AgentReadable,
    AgentHidden,
    LeaseOnly,
}
