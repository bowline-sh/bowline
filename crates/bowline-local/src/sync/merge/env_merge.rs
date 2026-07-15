use std::collections::BTreeMap;

use crate::{
    env::{EnvLineKind, parse_env_text},
    policy::is_project_env_name,
};

use super::super::line_merge::{TextMergeOutcome, merge_text_lines};

pub(super) enum EnvMergeOutcome {
    KeyConflict,
    Text(TextMergeOutcome),
}

pub(crate) fn is_env_path(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(is_project_env_name)
}

pub(super) fn merge_env_bytes(
    path: &str,
    base: &[u8],
    local: &[u8],
    remote: &[u8],
) -> EnvMergeOutcome {
    if !env_keys_can_merge(path, base, local, remote) {
        return EnvMergeOutcome::KeyConflict;
    }
    EnvMergeOutcome::Text(merge_text_lines(base, local, remote))
}

fn env_keys_can_merge(path: &str, base: &[u8], local: &[u8], remote: &[u8]) -> bool {
    let base = parse_env_text(path, "project", base);
    let local = parse_env_text(path, "project", local);
    let remote = parse_env_text(path, "project", remote);
    let base_values = env_values(&base);
    let local_values = env_values(&local);
    let remote_values = env_values(&remote);
    let all_keys = base_values
        .keys()
        .chain(local_values.keys())
        .chain(remote_values.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    for key in &all_keys {
        let base_value = base_values.get(key);
        let local_value = local_values.get(key);
        let remote_value = remote_values.get(key);
        if local_value == remote_value {
            continue;
        }
        if local_value == base_value {
            if remote_value.is_none() && key_has_multiple_occurrences(&all_keys, key) {
                return false;
            }
            continue;
        }
        if remote_value == base_value {
            if local_value.is_none() && key_has_multiple_occurrences(&all_keys, key) {
                return false;
            }
            continue;
        }
        return false;
    }

    true
}

fn key_has_multiple_occurrences(
    all_keys: &std::collections::BTreeSet<(String, usize)>,
    key: &(String, usize),
) -> bool {
    all_keys
        .iter()
        .filter(|candidate| candidate.0 == key.0)
        .count()
        > 1
}

fn env_values(parsed: &crate::env::ParsedEnvFile) -> BTreeMap<(String, usize), Vec<u8>> {
    parsed
        .lines
        .iter()
        .filter_map(|line| match &line.kind {
            EnvLineKind::KeyValue(value) => Some((
                (value.key.clone(), value.occurrence_index),
                value.value.as_bytes().to_vec(),
            )),
            EnvLineKind::Blank | EnvLineKind::Comment | EnvLineKind::Opaque(_) => None,
        })
        .collect()
}
