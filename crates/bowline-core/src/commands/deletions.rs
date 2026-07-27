//! Command contract for the push-side deletion breaker: read the removal batch
//! sync is refusing to publish, and authorise exactly one push of it.

use serde::{Deserialize, Serialize};

use crate::status::RepairCommand;

use super::CommandName;

/// Whether the engine is refusing a removal batch right now.
///
/// A state machine rather than an absent field: `clear` is an ordinary answer
/// ("nothing is waiting on you"), and a caller that had to infer it from a zero
/// count could not tell it apart from a block it failed to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionsState {
    /// Nothing is refused; sync is publishing deletions normally.
    Clear,
    /// A removal batch is refused and sync is publishing nothing until it is
    /// confirmed.
    Blocked,
}

/// What `bowline deletions --confirm` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionsConfirmation {
    /// One push may now publish the refused batch.
    Authorized,
    /// Nothing was refused, so nothing was authorised. A success: confirming a
    /// batch the engine already stopped refusing changes nothing, and a script
    /// that cannot know whether the guard fired must be able to run this safely.
    NotBlocked,
}

/// The refused removal batch, and the arithmetic the guard performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedDeletionBatch {
    /// Files and directories the refused push would have deleted everywhere.
    pub removals: u64,
    /// Entries the workspace currently syncs, which the ceiling is derived from.
    pub entries: u64,
    /// The largest removal batch one push may publish without confirmation.
    pub threshold: u64,
    /// A bounded, sorted sample of the refused paths. A refusal can name every
    /// entry in a workspace; `removals` is the magnitude, this is the evidence.
    pub paths: Vec<String>,
    /// How many of `removals` this response lists. Below `removals` means the
    /// sample was capped, never that the batch shrank.
    pub listed: u64,
}

/// The daemon's answer to "what is refused right now". Lives here rather than in
/// the daemon so the RPC and the CLI contract share one definition of the batch:
/// a second copy would be two vocabularies for one guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedDeletionsReport {
    pub state: DeletionsState,
    /// Present exactly when `state` is `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedDeletionBatch>,
}

/// The daemon's answer to "authorise it". Carries the batch it released, read
/// from the same engine state that decided the answer, so a caller never has to
/// pair it with a separate read that may already be stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionsConfirmationReport {
    pub state: DeletionsConfirmation,
    /// Present exactly when `state` is `authorized`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedDeletionBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionsCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub state: DeletionsState,
    /// Whether this invocation changed anything. False for the read-only
    /// preview, and the field a caller checks rather than re-deriving intent
    /// from the flags it passed.
    pub changed: bool,
    /// Present exactly when `state` is `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedDeletionBatch>,
    /// Present only for `--confirm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<DeletionsConfirmation>,
    pub next_actions: Vec<RepairCommand>,
}
