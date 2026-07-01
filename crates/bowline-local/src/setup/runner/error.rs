use super::*;

#[derive(Debug)]
pub enum SetupRunError {
    Metadata(MetadataError),
    Io(io::Error),
    Recipe(crate::setup::SetupRecipeError),
    Inference(SetupInferenceError),
    UnsafeWorkspacePath(String),
    MissingWorkspace,
    MissingRoot,
    MissingProject(String),
    Json(serde_json::Error),
    Events(LocalEventError),
}

impl fmt::Display for SetupRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "setup run failed: {error}"),
            Self::Recipe(error) => error.fmt(formatter),
            Self::Inference(error) => error.fmt(formatter),
            Self::UnsafeWorkspacePath(path) => {
                write!(
                    formatter,
                    "setup path {path} is not a normal directory below the accepted workspace"
                )
            }
            Self::MissingWorkspace => formatter.write_str("bowline workspace is not initialized"),
            Self::MissingRoot => formatter.write_str("bowline workspace root is not initialized"),
            Self::MissingProject(path) => {
                write!(formatter, "no bowline project found for {path}")
            }
            Self::Json(error) => write!(formatter, "setup receipt JSON failed: {error}"),
            Self::Events(error) => error.fmt(formatter),
        }
    }
}

impl Error for SetupRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Recipe(error) => Some(error),
            Self::Inference(error) => Some(error),
            Self::UnsafeWorkspacePath(_) => None,
            Self::Json(error) => Some(error),
            Self::Events(error) => Some(error),
            Self::MissingWorkspace | Self::MissingRoot | Self::MissingProject(_) => None,
        }
    }
}

impl From<MetadataError> for SetupRunError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for SetupRunError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::setup::SetupRecipeError> for SetupRunError {
    fn from(error: crate::setup::SetupRecipeError) -> Self {
        Self::Recipe(error)
    }
}

impl From<SetupInferenceError> for SetupRunError {
    fn from(error: SetupInferenceError) -> Self {
        Self::Inference(error)
    }
}

impl From<serde_json::Error> for SetupRunError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<LocalEventError> for SetupRunError {
    fn from(error: LocalEventError) -> Self {
        Self::Events(error)
    }
}
