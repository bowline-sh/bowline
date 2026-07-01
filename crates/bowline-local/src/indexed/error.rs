use super::*;

#[derive(Debug)]
pub enum IndexedError {
    Io(io::Error),
    Json(serde_json::Error),
    Metadata(MetadataError),
    MissingPath(PathBuf),
    NotDirectory(PathBuf),
}

impl fmt::Display for IndexedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "index filesystem operation failed: {error}"),
            Self::Json(error) => write!(formatter, "index pack payload was invalid: {error}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::MissingPath(path) => {
                write!(formatter, "indexed path does not exist: {}", path.display())
            }
            Self::NotDirectory(path) => write!(
                formatter,
                "indexed path is not a directory: {}",
                path.display()
            ),
        }
    }
}

impl Error for IndexedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Metadata(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for IndexedError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MetadataError> for IndexedError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<serde_json::Error> for IndexedError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
