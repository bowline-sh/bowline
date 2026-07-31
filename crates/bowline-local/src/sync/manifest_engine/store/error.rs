use std::error::Error;
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum ManifestStoreError {
    Sqlite(rusqlite::Error),
    Io(io::Error),
    Corrupt { field: &'static str },
    ValueOutOfRange { field: &'static str },
}

impl fmt::Display for ManifestStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "manifest store SQLite failed: {error}"),
            Self::Io(error) => write!(formatter, "manifest store I/O failed: {error}"),
            Self::Corrupt { field } => write!(formatter, "manifest store corrupt value: {field}"),
            Self::ValueOutOfRange { field } => {
                write!(formatter, "manifest store value out of range: {field}")
            }
        }
    }
}

impl Error for ManifestStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Corrupt { .. } | Self::ValueOutOfRange { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for ManifestStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<io::Error> for ManifestStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
