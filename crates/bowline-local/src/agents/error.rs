use super::*;

#[derive(Debug)]
pub enum AgentError {
    MissingWorkspace,
    MissingProject { path: String },
    MissingLease { lease_id: LeaseId },
    MissingWorkView { id: String },
    InvalidLease { reason: String },
    ToolDenied { code: String },
    Metadata(MetadataError),
    WorkView(WorkViewError),
    Event(LocalEventError),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWorkspace => write!(formatter, "no bowline workspace is initialized"),
            Self::MissingProject { path } => {
                write!(formatter, "no tracked project was found for `{path}`")
            }
            Self::MissingLease { lease_id } => write!(
                formatter,
                "agent lease `{}` was not found",
                lease_id.as_str()
            ),
            Self::MissingWorkView { id } => {
                write!(formatter, "lease work view `{id}` was not found")
            }
            Self::InvalidLease { reason } => write!(formatter, "agent lease is invalid: {reason}"),
            Self::ToolDenied { code } => write!(formatter, "agent tool denied: {code}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::WorkView(error) => error.fmt(formatter),
            Self::Event(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "agent file operation failed: {error}"),
            Self::Json(error) => write!(formatter, "agent JSON operation failed: {error}"),
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::WorkView(error) => Some(error),
            Self::Event(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MetadataError> for AgentError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<WorkViewError> for AgentError {
    fn from(error: WorkViewError) -> Self {
        Self::WorkView(error)
    }
}

impl From<LocalEventError> for AgentError {
    fn from(error: LocalEventError) -> Self {
        Self::Event(error)
    }
}

impl From<io::Error> for AgentError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
