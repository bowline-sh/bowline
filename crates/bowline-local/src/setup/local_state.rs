use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupReceiptIdentityInputs {
    pub os: String,
    pub arch: String,
    pub env_profile: String,
    pub recipe_hash: Option<String>,
    pub lockfiles: Vec<FileIdentity>,
    pub toolchains: Vec<FileIdentity>,
    pub package_manager: Option<PackageManagerIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentity {
    pub path: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerIdentity {
    pub name: String,
    pub command: String,
    pub declared: Option<String>,
    pub resolved_path: Option<PathBuf>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRegenerateOutput {
    pub path: String,
    pub kind: LocalRegenerateKind,
    pub produced_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalRegenerateKind {
    Dependency,
    Generated,
    Cache,
}

pub fn collect_receipt_identity_inputs(
    project_root: impl AsRef<Path>,
    env_profile: impl Into<String>,
    recipe_hash: Option<String>,
    package_manager: Option<PackageManagerIdentity>,
) -> io::Result<SetupReceiptIdentityInputs> {
    let project_root = project_root.as_ref();
    Ok(SetupReceiptIdentityInputs {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: env_profile.into(),
        recipe_hash,
        lockfiles: existing_file_identities(project_root, LOCKFILES)?,
        toolchains: existing_file_identities(project_root, TOOLCHAIN_FILES)?,
        package_manager,
    })
}

pub(crate) fn file_hash(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(format!("b3_{}", blake3::hash(&bytes).to_hex()))
}

fn existing_file_identities(root: &Path, names: &[&str]) -> io::Result<Vec<FileIdentity>> {
    let mut identities = Vec::new();
    for name in names {
        let path = root.join(name);
        if path.is_file() {
            identities.push(FileIdentity {
                path: (*name).to_string(),
                hash: file_hash(&path)?,
            });
        }
    }
    Ok(identities)
}

const LOCKFILES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "bun.lock",
    "bun.lockb",
    "uv.lock",
    "Cargo.lock",
    "go.sum",
];

const TOOLCHAIN_FILES: &[&str] = &[
    ".node-version",
    ".tool-versions",
    "mise.toml",
    "rust-toolchain.toml",
    "go.mod",
    "package.json",
    "pyproject.toml",
];
