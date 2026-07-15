use std::{error::Error, fmt, io};

use super::MetadataError;

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "metadata I/O failed: {error}"),
            Self::Sqlite(error) => write!(formatter, "metadata SQLite failed: {error}"),
            Self::InvalidStorageMetadata(reason) => {
                write!(formatter, "invalid storage metadata: {reason}")
            }
            Self::InvalidCurrentNamespaceProjection { field, reason } => write!(
                formatter,
                "invalid current-namespace projection {field}: {reason}"
            ),
            Self::ImmutableBindingConflict { logical_id, field } => write!(
                formatter,
                "immutable metadata binding `{logical_id}` conflicts on {field}"
            ),
            Self::IncompleteSnapshotRoot {
                snapshot_id,
                logical_id,
            } => write!(
                formatter,
                "snapshot root `{}` is incomplete: required logical record `{logical_id}` is not verified",
                snapshot_id.as_str()
            ),
            Self::FutureIncompatible { found, supported } => write!(
                formatter,
                "metadata schema version {found} is newer than supported version {supported}"
            ),
            Self::UnsupportedSchema => write!(
                formatter,
                "metadata database uses an unsupported schema; remove the local metadata database and re-run bowline login"
            ),
        }
    }
}

impl Error for MetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidStorageMetadata(_)
            | Self::InvalidCurrentNamespaceProjection { .. }
            | Self::ImmutableBindingConflict { .. }
            | Self::IncompleteSnapshotRoot { .. }
            | Self::FutureIncompatible { .. }
            | Self::UnsupportedSchema => None,
        }
    }
}

impl From<io::Error> for MetadataError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for MetadataError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
