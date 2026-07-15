use std::{io, path::Path};

use bowline_core::ids::{ProjectId, WorkspaceId};

use super::{
    PackageManagerIdentity, SetupCommandPlan, SetupReceiptIdentityInputs, SetupRecipeCommand,
    collect_receipt_identity_inputs, redact_setup_text,
};

pub use bowline_core::status::ProjectSetupReadinessState as SetupReadinessState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupIdentity {
    pub inputs: SetupReceiptIdentityInputs,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupReadinessClassification {
    pub state: SetupReadinessState,
    pub reason: String,
    pub remedy: Option<String>,
}

pub fn collect_setup_identity(
    project_root: impl AsRef<Path>,
    env_profile: impl Into<String>,
    recipe_hash: Option<String>,
    package_manager: Option<PackageManagerIdentity>,
) -> io::Result<SetupIdentity> {
    let inputs =
        collect_receipt_identity_inputs(project_root, env_profile, recipe_hash, package_manager)?;
    let hash = setup_identity_hash(&inputs)?;
    Ok(SetupIdentity { inputs, hash })
}

pub fn setup_identity_hash(inputs: &SetupReceiptIdentityInputs) -> io::Result<String> {
    let bytes = serde_json::to_vec(inputs).map_err(io::Error::other)?;
    Ok(format!("setupid_{}", blake3::hash(&bytes).to_hex()))
}

pub fn setup_receipt_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    recipe_hash: &str,
    receipt_key: &str,
) -> String {
    let input = format!(
        "{}:{}:{}:{}",
        workspace_id.as_str(),
        project_id.as_str(),
        recipe_hash,
        receipt_key
    );
    format!("setup_{}", blake3::hash(input.as_bytes()).to_hex())
}

pub fn inferred_receipt_key(command: &SetupCommandPlan, command_text: &str) -> io::Result<String> {
    let recipe_hash = inferred_recipe_hash(command);
    let identity = collect_receipt_identity_inputs(
        &command.cwd,
        "default",
        Some(recipe_hash),
        Some(command.package_manager.clone()),
    )?;
    let identity_json = serde_json::to_string(&identity).map_err(io::Error::other)?;
    let identity_hash = blake3::hash(identity_json.as_bytes());
    Ok(format!(
        "lockfile:{}:identity:{}:{}",
        command.lockfile,
        identity_hash.to_hex(),
        command_text
    ))
}

pub fn inferred_recipe_hash(command: &SetupCommandPlan) -> String {
    if command.lockfile.starts_with("toolchain:") {
        "inferred:toolchains".to_string()
    } else {
        format!("inferred:{}", command.lockfile)
    }
}

pub fn recipe_receipt_key(command: &SetupRecipeCommand, recipe_hash: &str) -> io::Result<String> {
    let identity = collect_receipt_identity_inputs(
        &command.cwd,
        "default",
        Some(recipe_hash.to_string()),
        None,
    )?;
    let identity_json = serde_json::to_string(&identity).map_err(io::Error::other)?;
    let identity_hash = blake3::hash(identity_json.as_bytes());
    Ok(format!(
        "line:{}:{}:identity:{}",
        command.line_number,
        command.command,
        identity_hash.to_hex()
    ))
}

pub fn classify_setup_command_result(
    command_text: &str,
    exit_code: Option<i32>,
    redacted_output: &str,
    output_limit_exceeded: bool,
) -> SetupReadinessClassification {
    if output_limit_exceeded {
        return SetupReadinessClassification {
            state: SetupReadinessState::Blocked,
            reason: "Setup output exceeded the local capture limit.".to_string(),
            remedy: Some("Rerun setup locally after reducing command output.".to_string()),
        };
    }

    if let Some(executable) = missing_executable(command_text, exit_code, redacted_output) {
        return SetupReadinessClassification {
            state: SetupReadinessState::Blocked,
            reason: format!("Required setup executable `{executable}` is not available."),
            remedy: Some(format!(
                "Install `{executable}` on this machine, then rerun setup for the hot project."
            )),
        };
    }

    SetupReadinessClassification {
        state: SetupReadinessState::Blocked,
        reason: "Setup command failed; output is redacted.".to_string(),
        remedy: Some("Inspect the redacted setup log and rerun setup after fixing the local machine dependency.".to_string()),
    }
}

fn missing_executable(
    command_text: &str,
    exit_code: Option<i32>,
    redacted_output: &str,
) -> Option<String> {
    if exit_code != Some(127) {
        return None;
    }
    let lowered_output = redacted_output.to_ascii_lowercase();
    if !(lowered_output.contains("command not found")
        || lowered_output.contains("not found")
        || lowered_output.contains("not recognized"))
    {
        return None;
    }
    if let Some(executable) = missing_executable_from_output(redacted_output) {
        return Some(executable);
    }
    shell_executable(command_text)
        .as_deref()
        .map(redact_setup_text)
        .map(|redacted| redacted.text)
}

fn missing_executable_from_output(redacted_output: &str) -> Option<String> {
    for line in redacted_output.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(candidate) = command_before_suffix(line, &lower, ": command not found")
            .or_else(|| command_before_suffix(line, &lower, ": not found"))
            .or_else(|| command_before_suffix(line, &lower, " is not recognized"))
        {
            let command = sanitize_executable_candidate(candidate);
            if !command.is_empty() {
                return Some(redact_setup_text(&command).text);
            }
        }
    }
    None
}

fn command_before_suffix<'a>(line: &'a str, lower: &str, suffix: &str) -> Option<&'a str> {
    let end = lower.find(suffix)?;
    let before = line.get(..end)?.trim();
    let candidate = before.rsplit(':').next().unwrap_or(before).trim();
    Some(candidate)
}

fn sanitize_executable_candidate(candidate: &str) -> String {
    candidate
        .trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '`' | '(' | ')' | ';' | '&' | '|' | '<' | '>'
            )
        })
        .trim()
        .to_string()
}

fn shell_executable(command_text: &str) -> Option<String> {
    command_text
        .split_whitespace()
        .find(|part| {
            !part.contains('=')
                && !matches!(
                    *part,
                    "env"
                        | "command"
                        | "exec"
                        | "sudo"
                        | "time"
                        | "nohup"
                        | "cd"
                        | "&&"
                        | "||"
                        | ";"
                        | "("
                        | ")"
                        | "{"
                        | "}"
                        | "if"
                        | "then"
                        | "else"
                        | "fi"
                )
        })
        .map(|part| {
            part.trim_matches(|character: char| {
                matches!(character, '"' | '\'' | '(' | ')' | ';' | '&' | '|')
            })
            .to_string()
        })
        .filter(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::workspace::TempWorkspace;

    use super::*;

    #[test]
    fn setup_identity_hash_is_path_stable_and_content_sensitive() {
        let first = TempWorkspace::new("setup-identity-first").expect("first temp");
        let second = TempWorkspace::new("setup-identity-second").expect("second temp");
        for root in [first.root(), second.root()] {
            fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").expect("lockfile");
            fs::write(
                root.join("package.json"),
                "{\"packageManager\":\"pnpm@10.30.0\"}\n",
            )
            .expect("package");
            fs::write(root.join(".node-version"), "24\n").expect("node version");
        }

        let first_identity = collect_setup_identity(
            first.root(),
            "default",
            Some("inferred:pnpm-lock.yaml".to_string()),
            None,
        )
        .expect("first identity");
        let second_identity = collect_setup_identity(
            second.root(),
            "default",
            Some("inferred:pnpm-lock.yaml".to_string()),
            None,
        )
        .expect("second identity");
        assert_eq!(first_identity.hash, second_identity.hash);

        fs::write(
            second.root().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.1'\n",
        )
        .expect("changed lockfile");
        let changed_identity = collect_setup_identity(
            second.root(),
            "default",
            Some("inferred:pnpm-lock.yaml".to_string()),
            None,
        )
        .expect("changed identity");
        assert_ne!(first_identity.hash, changed_identity.hash);
    }

    #[test]
    fn command_not_found_classifies_as_blocked_with_redacted_executable() {
        let classification = classify_setup_command_result(
            "pnpm install --frozen-lockfile",
            Some(127),
            "sh: pnpm: command not found",
            false,
        );

        assert_eq!(classification.state, SetupReadinessState::Blocked);
        assert!(classification.reason.contains("`pnpm`"));
        assert!(
            classification
                .remedy
                .as_deref()
                .is_some_and(|remedy| remedy.contains("Install `pnpm`"))
        );
    }

    #[test]
    fn compound_command_not_found_uses_output_executable() {
        let classification = classify_setup_command_result(
            "cd frontend && pnpm install --frozen-lockfile",
            Some(127),
            "sh: pnpm: command not found",
            false,
        );

        assert_eq!(classification.state, SetupReadinessState::Blocked);
        assert!(classification.reason.contains("`pnpm`"));
    }
}
