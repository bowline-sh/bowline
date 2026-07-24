mod paths;
mod schema;
mod sqlite;
mod store;

pub(crate) use store::EnvRecordSourceReplacement;

pub use bowline_core::workspace_graph::WorkspaceRelativePath;
pub use paths::{
    Platform, control_socket_path_for_platform, database_path_for_platform,
    default_control_socket_path, default_database_path, default_state_root,
    state_root_for_platform,
};
pub use store::{
    DatabaseInspection, DatabaseState, EnvRecord, LocalPathRecord, MetadataError, MetadataStore,
    ObservedLocalPath, ProjectLifecycleState, ProjectLocalMaterializationState, ProjectRecord,
    ProjectUpsert, SetupReceiptRecord, WorkViewRecord, WorkspaceRecord, all_accepted_roots,
};

pub const DEFAULT_DATABASE_FILE: &str = "local.sqlite3";
pub const DEFAULT_CONTROL_SOCKET_FILE: &str = "bowline-daemon.sock";
