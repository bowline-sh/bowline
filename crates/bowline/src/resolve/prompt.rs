use super::*;

pub(super) fn mark_bundle_state(
    bundle: &Path,
    conflict: &ResolveConflict,
    state: ConflictState,
    generated_at: &str,
) -> Result<(), ResolveError> {
    transition_conflict_occurrence_state(
        bundle,
        &conflict.id,
        conflict.occurrence_version,
        state,
        generated_at,
    )
    .map_err(|error| ResolveError::Io(io::Error::other(error)))?
    .then_some(())
    .ok_or_else(|| ResolveError::ConflictNotFound(conflict.id.clone()))
}

pub(super) fn build_prompt(conflict: &ResolveConflict) -> ResolvePrompt {
    let resolution_path = Path::new(&conflict.bundle_path)
        .join("resolution")
        .display()
        .to_string();
    let affected = if conflict.affected_files.is_empty() {
        "No affected file list is available in manifest.json.".to_string()
    } else {
        conflict.affected_files.join(", ")
    };
    let secret_note = if conflict.contains_secrets {
        "The bundle is marked secret-bearing. Do not print file contents or environment values."
    } else {
        "Do not print secrets, environment values, tokens, or private file contents."
    };
    let span_note = if conflict.spans.is_empty() {
        "No precise conflict spans were recorded; inspect each affected file as an opaque conflict."
            .to_string()
    } else {
        format!("Conflict spans:\n{}", format_spans(&conflict.spans))
    };
    let kind_note = prompt_kind_note(conflict);
    let text = format!(
        "Repair bowline conflict `{}`.\n\nConflict kind: {}\n{}\nBundle path: {}\nAffected files: {}\n{}\n\nLayout:\n- base/ contains the common ancestor bytes.\n- local/ contains this device's version.\n- remote/ contains the workspace-head version from the other device.\n- resolution/ is the only place you may write repaired files.\n\nRules:\n- Do not use Git, mutate Git state, create branches, stage files, commit, push, or publish.\n- Do not write outside the resolution overlay.\n- Do not modify base/, local/, remote/, manifest.json, or the live project path.\n- Write the final repaired file contents under resolution/ using the same relative paths.\n- {}\n",
        conflict.id,
        conflict.conflict_kind,
        kind_note,
        conflict.bundle_path,
        affected,
        span_note,
        secret_note
    );

    ResolvePrompt {
        conflict_id: conflict.id.clone(),
        bundle_path: conflict.bundle_path.clone(),
        resolution_path,
        redaction: "applied",
        text,
    }
}

pub(super) fn prompt_kind_note(conflict: &ResolveConflict) -> &'static str {
    match conflict.conflict_kind.as_str() {
        "structured-text" => {
            "The previous automatic merge failed structured validation; keep the final file parseable."
        }
        "opaque-git" => {
            "This is opaque Git state. Do not run Git repair commands; preserve the intended bytes under resolution/."
        }
        "delete-edit" => "One side deleted the path while the other edited it.",
        "path-shape" => {
            "The path shape differs between sides; do not replace directories or symlinks blindly."
        }
        "env-key" => {
            "This is an env key conflict. Do not copy secret values into the prompt or response."
        }
        _ => "Resolve only the unsafe overlap; preserve unrelated safe edits.",
    }
}

pub(super) fn format_spans(spans: &[ResolveConflictSpan]) -> String {
    spans
        .iter()
        .map(|span| {
            let anchor = match (
                span.base_context_hash.as_deref(),
                span.local_context_hash.as_deref(),
                span.remote_context_hash.as_deref(),
            ) {
                (Some(base), Some(local), Some(remote)) => {
                    format!(" anchors base:{base} local:{local} remote:{remote}")
                }
                _ => String::new(),
            };
            format!(
                "- {} base:{}-{} local:{}-{} remote:{}-{}{}",
                span.path,
                span.base_start_line,
                span.base_end_line,
                span.local_start_line,
                span.local_end_line,
                span.remote_start_line,
                span.remote_end_line,
                anchor,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn build_diff(conflict: &ResolveConflict) -> ResolveDiff {
    let bundle = Path::new(&conflict.bundle_path);
    let affected = if conflict.affected_files.is_empty() {
        "No affected file list is available in manifest.json.".to_string()
    } else {
        conflict.affected_files.join(", ")
    };
    let text = format!(
        "Conflict diff for `{}`\n\nBundle path: {}\nAffected files: {}\n\nReview paths:\n- base: {}\n- local: {}\n- remote: {}\n- resolution: {}\n\nRedaction: file contents are not printed here. Open these bundle paths locally to inspect bytes, or use copy-prompt to hand the bundle to an agent.\n",
        conflict.id,
        conflict.bundle_path,
        affected,
        bundle.join("base").display(),
        bundle.join("local").display(),
        bundle.join("remote").display(),
        bundle.join("resolution").display()
    );

    ResolveDiff {
        conflict_id: conflict.id.clone(),
        bundle_path: conflict.bundle_path.clone(),
        redaction: "contents-not-printed",
        affected_files: conflict.affected_files.clone(),
        text,
    }
}
