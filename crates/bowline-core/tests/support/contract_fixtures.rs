use std::{fs, path::PathBuf};

use serde::Deserialize;
use serde_json::Value;

pub fn fixture_json(relative_path: &str) -> Value {
    serde_json::from_str(&fixture_text(relative_path)).expect("fixture is valid JSON")
}

pub fn fixture_text(relative_path: &str) -> String {
    let path = fixtures_dir().join(relative_path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("fixture is readable at {}: {error}", path.display()))
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts")
}

pub fn manifest_entries_for_rust(family: &str, decoder: Option<&str>) -> Vec<ManifestFixture> {
    let fixtures = contract_manifest()
        .fixtures
        .into_iter()
        .filter(|fixture| {
            fixture.family == family
                && match decoder {
                    Some(decoder) => fixture.language_decoders.rust.as_deref() == Some(decoder),
                    None => fixture.language_decoders.rust.is_some(),
                }
        })
        .collect::<Vec<_>>();
    assert!(
        !fixtures.is_empty(),
        "manifest should list {family} fixtures for Rust decoder {decoder:?}"
    );
    fixtures
}

pub fn contract_manifest() -> ContractManifest {
    let path = fixtures_dir().join("manifest.json");
    let json = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("manifest is readable at {}: {error}", path.display()));
    let manifest: ContractManifest =
        serde_json::from_str(&json).expect("contract manifest is valid JSON");
    assert_eq!(manifest.manifest_version, 1);
    manifest
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractManifest {
    pub manifest_version: u16,
    pub fixtures: Vec<ManifestFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFixture {
    pub id: String,
    pub family: String,
    pub path: String,
    pub format: String,
    pub kind: String,
    pub language_decoders: LanguageDecoders,
}

#[derive(Debug, Deserialize)]
pub struct LanguageDecoders {
    pub rust: Option<String>,
}
