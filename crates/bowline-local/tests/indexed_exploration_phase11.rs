use std::collections::BTreeSet;

use bowline_core::{
    commands::{IndexState, SymbolKind, SymbolLanguage},
    ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkspaceId},
    policy::PathClassification,
    workspace_graph::{HydrationState, NamespaceEntryKind},
};
use bowline_local::{
    indexed::{DecryptedIndexPackImport, IndexedProjectIdentity, import_decrypted_index_pack},
    metadata::{IndexWorkRecord, LocalWriteLogRecord, MetadataStore, ProjectedNodeRecord},
    search::{SearchCommandOptions, search_workspace},
    symbols::{SymbolCommandOptions, lookup_symbols},
    workspace::TempWorkspace,
};
use bowline_storage::{StorageKey, open_index_pack, seal_index_pack};

#[path = "indexed_exploration_phase11/durable.rs"]
mod durable;
#[path = "indexed_exploration_phase11/packs.rs"]
mod packs;
#[path = "indexed_exploration_phase11/search.rs"]
mod search;
#[path = "indexed_exploration_phase11/symbols.rs"]
mod symbols;
