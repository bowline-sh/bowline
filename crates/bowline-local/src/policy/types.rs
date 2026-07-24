use bowline_core::policy::{AccessFlag, MaterializationMode, PathClassification};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFacts {
    pub relative_path: String,
    pub is_dir: bool,
    pub byte_len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPolicyDecision {
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
}
