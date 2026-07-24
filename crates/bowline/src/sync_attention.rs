use super::*;

use serde::Serialize;

// The old durable engine parked failed operations in an attention store and
// these commands drove manual recovery. The manifest engine retries
// automatically and parks nothing (Plan 111), so the surviving surface is a
// truthful empty view: `sync attention` always reports zero incidents and the
// per-incident actions report that nothing is parked. The menu bar keeps
// consuming the same JSON shape.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RetrySelector {
    Incident(String),
    All,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncAttentionOutput {
    contract_version: u16,
    generated_at: String,
    incidents: Vec<()>,
}

pub(super) fn print_sync_attention(json: bool, _socket: &Path) -> ExitCode {
    let output = SyncAttentionOutput {
        contract_version: CONTRACT_VERSION,
        generated_at: generated_at(),
        incidents: Vec::new(),
    };
    if json {
        print_json(&output);
    } else {
        println!("No parked sync operations; the sync engine retries automatically.");
    }
    ExitCode::SUCCESS
}

pub(super) fn print_sync_retry(_selector: RetrySelector, json: bool, socket: &Path) -> ExitCode {
    print_sync_attention(json, socket)
}

pub(super) fn print_sync_dismiss(_incident_id: String, json: bool, socket: &Path) -> ExitCode {
    print_sync_attention(json, socket)
}
