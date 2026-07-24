use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::fs_atomic::{AtomicWriteOptions, sync_parent_for_path, write_atomic};
use zeroize::Zeroizing;

use crate::{
    StorageKey,
    envelope::{EnvelopeError, open_with_associated_data, seal_with_associated_data},
};

mod identity;

pub use identity::{
    LocalRecoveryEpochIdentity, LocalRecoveryExpectedPreimageIdentity, LocalRecoveryKeyEpoch,
    LocalRecoveryPlaintextLocator, LocalRecoveryPreimageContext, LocalRecoveryPreimageLocator,
    LocalRecoveryWorkspacePath,
};

#[derive(Debug)]
pub struct SealLocalRecoveryPreimageRequest<'a> {
    pub plaintext_root: &'a Path,
    pub sealed_state_root: &'a Path,
    pub key: StorageKey,
    pub context: &'a LocalRecoveryPreimageContext,
}

#[derive(Debug)]
pub struct OpenLocalRecoveryPreimageRequest<'a> {
    pub sealed_state_root: &'a Path,
    pub sealed_locator: &'a LocalRecoveryPreimageLocator,
    pub key: StorageKey,
    pub context: &'a LocalRecoveryPreimageContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedLocalRecoveryPreimage {
    locator: LocalRecoveryPreimageLocator,
    key_epoch: LocalRecoveryKeyEpoch,
}

impl SealedLocalRecoveryPreimage {
    pub fn locator(&self) -> &LocalRecoveryPreimageLocator {
        &self.locator
    }

    pub const fn key_epoch(&self) -> LocalRecoveryKeyEpoch {
        self.key_epoch
    }
}

pub fn seal_local_recovery_preimage(
    request: SealLocalRecoveryPreimageRequest<'_>,
) -> Result<SealedLocalRecoveryPreimage, LocalRecoveryPreimageError> {
    seal_local_recovery_preimage_with(request, |_| Ok(()))
}

fn seal_local_recovery_preimage_with(
    request: SealLocalRecoveryPreimageRequest<'_>,
    after_publish: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<SealedLocalRecoveryPreimage, LocalRecoveryPreimageError> {
    let sealed_locator = request.context.sealed_locator();
    let plaintext_result = read_stable_plaintext(
        request.plaintext_root,
        request.context.plaintext_locator().as_path(),
    );
    let (plaintext_path, plaintext_identity, plaintext) = match plaintext_result {
        Ok(plaintext) => plaintext,
        Err(plaintext_error) if plaintext_error.is_not_found() => {
            return match resume_authenticated_seal(&request, sealed_locator) {
                Ok(sealed) => Ok(sealed),
                Err(sealed_error) if sealed_error.is_not_found() => Err(plaintext_error),
                Err(sealed_error) => Err(sealed_error),
            };
        }
        Err(error) => return Err(error),
    };
    let sealed_path =
        prepare_local_recovery_file(request.sealed_state_root, sealed_locator.as_path())?;
    let associated_data = request.context.associated_data(sealed_locator)?;
    let sealed = seal_with_associated_data(
        plaintext.as_slice(),
        request.key,
        request.context.key_epoch().value(),
        &associated_data,
    )?;

    let write_result = write_atomic(
        &sealed_path,
        sealed.as_bytes(),
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: false,
        },
    );
    let published_new_envelope = match write_result {
        Ok(()) => true,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
        Err(source) => {
            return Err(LocalRecoveryPreimageError::Io {
                operation: "write_encrypted_preimage",
                source,
            });
        }
    };

    authenticate_existing_preimage(
        &sealed_path,
        plaintext.as_slice(),
        request.key,
        request.context,
        sealed_locator,
    )?;
    ensure_secure_file_mode(&sealed_path)?;
    after_publish(&plaintext_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "after_encrypted_preimage_publish",
        source,
    })?;
    if let Err(source) =
        verify_plaintext_unchanged(&plaintext_path, &plaintext_identity, plaintext.as_slice())
    {
        let encrypted_cleanup_error = published_new_envelope
            .then(|| remove_encrypted_after_plaintext_failure(&sealed_path))
            .flatten();
        return Err(LocalRecoveryPreimageError::PlaintextRevalidation {
            source: Box::new(source),
            encrypted_cleanup_error,
        });
    }

    if let Err(source) = fs::remove_file(&plaintext_path) {
        let encrypted_cleanup_error = published_new_envelope
            .then(|| remove_encrypted_after_plaintext_failure(&sealed_path))
            .flatten();
        return Err(LocalRecoveryPreimageError::PlaintextCleanup {
            source,
            encrypted_cleanup_error,
        });
    }
    sync_parent_for_path(&plaintext_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "fsync_plaintext_parent",
        source,
    })?;
    Ok(SealedLocalRecoveryPreimage {
        locator: sealed_locator.clone(),
        key_epoch: request.context.key_epoch(),
    })
}

fn resume_authenticated_seal(
    request: &SealLocalRecoveryPreimageRequest<'_>,
    sealed_locator: &LocalRecoveryPreimageLocator,
) -> Result<SealedLocalRecoveryPreimage, LocalRecoveryPreimageError> {
    let sealed_path = resolve_existing_regular_file(
        request.sealed_state_root,
        sealed_locator.as_path(),
        "resume_encrypted_preimage",
    )?;
    let envelope = fs::read(&sealed_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "resume_encrypted_preimage",
        source,
    })?;
    let associated_data = request.context.associated_data(sealed_locator)?;
    let _authenticated_plaintext = Zeroizing::new(open_with_associated_data(
        &envelope,
        request.key,
        request.context.key_epoch().value(),
        &associated_data,
    )?);
    ensure_secure_file_mode(&sealed_path)?;
    let plaintext_path = request
        .plaintext_root
        .join(request.context.plaintext_locator().as_path());
    sync_parent_for_path(&plaintext_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "fsync_plaintext_parent_on_resume",
        source,
    })?;
    Ok(SealedLocalRecoveryPreimage {
        locator: sealed_locator.clone(),
        key_epoch: request.context.key_epoch(),
    })
}

pub fn open_local_recovery_preimage(
    request: OpenLocalRecoveryPreimageRequest<'_>,
) -> Result<Vec<u8>, LocalRecoveryPreimageError> {
    let sealed_path = resolve_existing_regular_file(
        request.sealed_state_root,
        request.sealed_locator.as_path(),
        "read_encrypted_preimage",
    )?;
    let envelope = fs::read(sealed_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "read_encrypted_preimage",
        source,
    })?;
    let associated_data = request.context.associated_data(request.sealed_locator)?;
    open_with_associated_data(
        &envelope,
        request.key,
        request.context.key_epoch().value(),
        &associated_data,
    )
    .map_err(Into::into)
}

/// Resolves a private file path after securely creating every parent directory.
///
/// The component walk is required because recursive directory creation follows
/// symlinks and could redirect plaintext recovery state outside its trusted root.
pub fn prepare_local_recovery_file(
    state_root: &Path,
    locator: &Path,
) -> Result<PathBuf, LocalRecoveryPreimageError> {
    ensure_secure_directory(state_root, "prepare_state_root")?;
    let mut current = state_root.to_path_buf();
    if let Some(parent) = locator.parent() {
        for component in parent.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(LocalRecoveryPreimageError::InvalidLocator {
                    field: "locator",
                    reason: "must contain only normal path components",
                });
            };
            current.push(segment);
            ensure_secure_directory(&current, "prepare_encrypted_parent")?;
        }
    }
    Ok(state_root.join(locator))
}

fn resolve_existing_regular_file(
    state_root: &Path,
    locator: &Path,
    operation: &'static str,
) -> Result<PathBuf, LocalRecoveryPreimageError> {
    validate_existing_directory(state_root, operation)?;
    let mut current = state_root.to_path_buf();
    let components = locator.components().collect::<Vec<_>>();
    for component in &components[..components.len() - 1] {
        let std::path::Component::Normal(component) = component else {
            return Err(LocalRecoveryPreimageError::InvalidLocator {
                field: "locator",
                reason: "must contain only normal path components",
            });
        };
        current.push(component);
        validate_existing_directory(&current, operation)?;
    }
    let std::path::Component::Normal(file_name) = components[components.len() - 1] else {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field: "locator",
            reason: "must end in a normal path component",
        });
    };
    current.push(file_name);
    let metadata = fs::symlink_metadata(&current)
        .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(LocalRecoveryPreimageError::NotRegularFile { operation });
    }
    Ok(current)
}

fn validate_existing_directory(
    path: &Path,
    operation: &'static str,
) -> Result<(), LocalRecoveryPreimageError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(LocalRecoveryPreimageError::UnsafeDirectory { operation });
    }
    Ok(())
}

fn read_stable_plaintext(
    plaintext_root: &Path,
    locator: &Path,
) -> Result<(PathBuf, RegularFileIdentity, Zeroizing<Vec<u8>>), LocalRecoveryPreimageError> {
    let path = resolve_existing_regular_file(plaintext_root, locator, "read_plaintext")?;
    let before = regular_file_identity(&path, "read_plaintext")?;
    let plaintext =
        Zeroizing::new(
            fs::read(&path).map_err(|source| LocalRecoveryPreimageError::Io {
                operation: "read_plaintext",
                source,
            })?,
        );
    let after = regular_file_identity(&path, "read_plaintext")?;
    if before != after {
        return Err(LocalRecoveryPreimageError::PlaintextChanged);
    }
    Ok((path, after, plaintext))
}

fn verify_plaintext_unchanged(
    path: &Path,
    expected_identity: &RegularFileIdentity,
    expected_plaintext: &[u8],
) -> Result<(), LocalRecoveryPreimageError> {
    let before = regular_file_identity(path, "verify_plaintext")?;
    let current =
        Zeroizing::new(
            fs::read(path).map_err(|source| LocalRecoveryPreimageError::Io {
                operation: "verify_plaintext",
                source,
            })?,
        );
    let after = regular_file_identity(path, "verify_plaintext")?;
    if &before != expected_identity || after != before || current.as_slice() != expected_plaintext {
        return Err(LocalRecoveryPreimageError::PlaintextChanged);
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegularFileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    mode: u32,
}

#[cfg(not(unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegularFileIdentity {
    length: u64,
}

#[cfg(unix)]
fn regular_file_identity(
    path: &Path,
    operation: &'static str,
) -> Result<RegularFileIdentity, LocalRecoveryPreimageError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = regular_file_metadata(path, operation)?;
    Ok(RegularFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        mode: metadata.mode(),
    })
}

#[cfg(not(unix))]
fn regular_file_identity(
    path: &Path,
    operation: &'static str,
) -> Result<RegularFileIdentity, LocalRecoveryPreimageError> {
    let metadata = regular_file_metadata(path, operation)?;
    Ok(RegularFileIdentity {
        length: metadata.len(),
    })
}

fn regular_file_metadata(
    path: &Path,
    operation: &'static str,
) -> Result<fs::Metadata, LocalRecoveryPreimageError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(LocalRecoveryPreimageError::PlaintextChanged);
    }
    Ok(metadata)
}

fn ensure_secure_directory(
    path: &Path,
    operation: &'static str,
) -> Result<(), LocalRecoveryPreimageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(LocalRecoveryPreimageError::UnsafeDirectory { operation }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })?;
            sync_parent_for_path(path)
                .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })?;
        }
        Err(source) => return Err(LocalRecoveryPreimageError::Io { operation, source }),
    }
    set_secure_directory_mode(path, operation)?;
    sync_path(path, operation)
}

#[cfg(unix)]
fn set_secure_directory_mode(
    path: &Path,
    operation: &'static str,
) -> Result<(), LocalRecoveryPreimageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| LocalRecoveryPreimageError::Io { operation, source })
}

#[cfg(unix)]
fn ensure_secure_file_mode(path: &Path) -> Result<(), LocalRecoveryPreimageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        LocalRecoveryPreimageError::Io {
            operation: "secure_encrypted_preimage",
            source,
        }
    })?;
    sync_path(path, "secure_encrypted_preimage")
}

#[cfg(not(unix))]
fn ensure_secure_file_mode(_path: &Path) -> Result<(), LocalRecoveryPreimageError> {
    Ok(())
}

fn sync_path(path: &Path, operation: &'static str) -> Result<(), LocalRecoveryPreimageError> {
    match fs::File::open(path).and_then(|file| file.sync_all()) {
        Ok(()) => Ok(()),
        Err(source)
            if matches!(
                source.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(LocalRecoveryPreimageError::Io { operation, source }),
    }
}

#[cfg(not(unix))]
fn set_secure_directory_mode(
    _path: &Path,
    _operation: &'static str,
) -> Result<(), LocalRecoveryPreimageError> {
    Ok(())
}

fn remove_encrypted_after_plaintext_failure(path: &Path) -> Option<io::Error> {
    fs::remove_file(path)
        .and_then(|()| sync_parent_for_path(path))
        .err()
}

fn authenticate_existing_preimage(
    sealed_path: &Path,
    expected_plaintext: &[u8],
    key: StorageKey,
    context: &LocalRecoveryPreimageContext,
    locator: &LocalRecoveryPreimageLocator,
) -> Result<(), LocalRecoveryPreimageError> {
    let envelope = fs::read(sealed_path).map_err(|source| LocalRecoveryPreimageError::Io {
        operation: "verify_encrypted_preimage",
        source,
    })?;
    let associated_data = context.associated_data(locator)?;
    let opened = Zeroizing::new(open_with_associated_data(
        &envelope,
        key,
        context.key_epoch().value(),
        &associated_data,
    )?);
    if opened.as_slice() != expected_plaintext {
        return Err(LocalRecoveryPreimageError::ExistingPreimageMismatch);
    }
    Ok(())
}

#[derive(Debug)]
pub enum LocalRecoveryPreimageError {
    InvalidContext {
        field: &'static str,
        reason: &'static str,
    },
    InvalidLocator {
        field: &'static str,
        reason: &'static str,
    },
    ExistingPreimageMismatch,
    ContextLocatorMismatch,
    PlaintextChanged,
    UnsafeDirectory {
        operation: &'static str,
    },
    NotRegularFile {
        operation: &'static str,
    },
    ContextSerialization {
        source: serde_json::Error,
    },
    Envelope(EnvelopeError),
    Io {
        operation: &'static str,
        source: io::Error,
    },
    PlaintextCleanup {
        source: io::Error,
        encrypted_cleanup_error: Option<io::Error>,
    },
    PlaintextRevalidation {
        source: Box<LocalRecoveryPreimageError>,
        encrypted_cleanup_error: Option<io::Error>,
    },
}

impl fmt::Display for LocalRecoveryPreimageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidContext { field, reason } => {
                write!(
                    formatter,
                    "invalid local recovery context {field}: {reason}"
                )
            }
            Self::InvalidLocator { field, reason } => {
                write!(formatter, "invalid local recovery {field}: {reason}")
            }
            Self::ExistingPreimageMismatch => formatter.write_str(
                "existing encrypted recovery preimage does not match the plaintext transition",
            ),
            Self::ContextLocatorMismatch => formatter
                .write_str("encrypted recovery locator does not match its encryption context"),
            Self::PlaintextChanged => {
                formatter.write_str("local recovery plaintext changed before durable cleanup")
            }
            Self::UnsafeDirectory { operation } => {
                write!(
                    formatter,
                    "local recovery {operation} found an unsafe directory"
                )
            }
            Self::NotRegularFile { operation } => {
                write!(
                    formatter,
                    "local recovery {operation} requires a regular file"
                )
            }
            Self::ContextSerialization { .. } => {
                formatter.write_str("local recovery encryption context serialization failed")
            }
            Self::Envelope(source) => {
                write!(formatter, "local recovery encryption failed: {source}")
            }
            Self::Io { operation, .. } => write!(formatter, "local recovery {operation} failed"),
            Self::PlaintextCleanup {
                encrypted_cleanup_error,
                ..
            } => {
                formatter.write_str("local recovery plaintext cleanup failed")?;
                if encrypted_cleanup_error.is_some() {
                    formatter.write_str(" and encrypted rollback was incomplete")?;
                }
                Ok(())
            }
            Self::PlaintextRevalidation {
                encrypted_cleanup_error,
                ..
            } => {
                formatter.write_str("local recovery plaintext revalidation failed")?;
                if encrypted_cleanup_error.is_some() {
                    formatter.write_str(" and encrypted rollback was incomplete")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for LocalRecoveryPreimageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ContextSerialization { source } => Some(source),
            Self::Envelope(source) => Some(source),
            Self::Io { source, .. } | Self::PlaintextCleanup { source, .. } => Some(source),
            Self::PlaintextRevalidation { source, .. } => Some(source.as_ref()),
            Self::InvalidContext { .. }
            | Self::InvalidLocator { .. }
            | Self::ExistingPreimageMismatch
            | Self::ContextLocatorMismatch
            | Self::PlaintextChanged
            | Self::UnsafeDirectory { .. }
            | Self::NotRegularFile { .. } => None,
        }
    }
}

impl From<EnvelopeError> for LocalRecoveryPreimageError {
    fn from(source: EnvelopeError) -> Self {
        Self::Envelope(source)
    }
}

impl LocalRecoveryPreimageError {
    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
        )
    }
}

#[cfg(test)]
mod tests;
