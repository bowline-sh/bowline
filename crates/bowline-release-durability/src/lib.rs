//! Release-evidence durability barriers with a small, privacy-safe CLI contract.

use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;

use serde::Serialize;

pub const OUTPUT_SCHEMA_VERSION: u16 = 1;
pub const INHERITED_DESCRIPTOR_SLOT: i32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DurabilityOperation {
    SyncInheritedFd,
    Invalid,
}

impl DurabilityOperation {
    fn parse(value: &OsString) -> Option<Self> {
        match value.to_str() {
            Some("sync-inherited-fd") => Some(Self::SyncInheritedFd),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlatformContract {
    #[serde(rename = "darwin_f_fullfsync_durability_v1")]
    DarwinFullFsyncV1,
    #[serde(rename = "linux_fsync_durability_v1")]
    LinuxFsyncV1,
    #[serde(rename = "unsupported_durability_v1")]
    UnsupportedV1,
}

impl PlatformContract {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::DarwinFullFsyncV1
        } else if cfg!(target_os = "linux") {
            Self::LinuxFsyncV1
        } else {
            Self::UnsupportedV1
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityResult {
    Durable,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityFailureCode {
    InvalidInvocation,
    SymlinkRefused,
    WrongTargetType,
    DurabilityBarrierFailed,
    DirectoryPersistenceUnsupported,
    InheritedDescriptorUnavailable,
    PlatformUnsupported,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurabilityOutput {
    pub schema_version: u16,
    pub operation: DurabilityOperation,
    pub result: DurabilityResult,
    pub failure_code: Option<DurabilityFailureCode>,
    pub platform_contract: PlatformContract,
}

impl DurabilityOutput {
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self.result, DurabilityResult::Durable)
    }

    fn success(operation: DurabilityOperation) -> Self {
        Self {
            schema_version: OUTPUT_SCHEMA_VERSION,
            operation,
            result: DurabilityResult::Durable,
            failure_code: None,
            platform_contract: PlatformContract::current(),
        }
    }

    fn failure(operation: DurabilityOperation, failure_code: DurabilityFailureCode) -> Self {
        Self {
            schema_version: OUTPUT_SCHEMA_VERSION,
            operation,
            result: DurabilityResult::Failed,
            failure_code: Some(failure_code),
            platform_contract: PlatformContract::current(),
        }
    }
}

#[derive(Debug)]
pub struct Invocation {
    operation: DurabilityOperation,
    expected_directory: Option<bool>,
}

impl Invocation {
    #[must_use]
    pub fn parse<I>(arguments: I) -> Self
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut arguments = arguments.into_iter();
        let operation = arguments
            .next()
            .as_ref()
            .and_then(DurabilityOperation::parse)
            .unwrap_or(DurabilityOperation::Invalid);
        let expected_directory = match operation {
            DurabilityOperation::SyncInheritedFd => parse_inherited_target(&mut arguments),
            DurabilityOperation::Invalid => None,
        };
        Self {
            operation,
            expected_directory,
        }
    }

    #[must_use]
    pub fn execute(self) -> DurabilityOutput {
        let Some(expected_directory) = self.expected_directory else {
            return DurabilityOutput::failure(
                self.operation,
                DurabilityFailureCode::InvalidInvocation,
            );
        };
        let outcome = sync_inherited_descriptor(expected_directory);
        match outcome {
            Ok(()) => DurabilityOutput::success(self.operation),
            Err(failure_code) => DurabilityOutput::failure(self.operation, failure_code),
        }
    }
}

fn parse_inherited_target<I>(arguments: &mut I) -> Option<bool>
where
    I: Iterator<Item = OsString>,
{
    if arguments.next()?.to_str()? != "--kind" {
        return None;
    }
    let expected_directory = match arguments.next()?.to_str()? {
        "file" => false,
        "directory" => true,
        _ => return None,
    };
    if arguments.next()?.to_str()? != "--fd" {
        return None;
    }
    if arguments.next()?.to_str()? != "3" || arguments.next().is_some() {
        return None;
    }
    Some(expected_directory)
}

fn sync_inherited_descriptor(expected_directory: bool) -> Result<(), DurabilityFailureCode> {
    ensure_supported_platform()?;
    let file = duplicate_inherited_descriptor()?;
    let metadata = file
        .metadata()
        .map_err(|_| DurabilityFailureCode::InheritedDescriptorUnavailable)?;
    validate_type(&metadata, expected_directory)?;
    persist_descriptor(&file, expected_directory)
}

fn ensure_supported_platform() -> Result<(), DurabilityFailureCode> {
    if matches!(PlatformContract::current(), PlatformContract::UnsupportedV1) {
        Err(DurabilityFailureCode::PlatformUnsupported)
    } else {
        Ok(())
    }
}

fn duplicate_inherited_descriptor() -> Result<File, DurabilityFailureCode> {
    // SAFETY: F_DUPFD_CLOEXEC accepts an integer descriptor and returns either a
    // newly owned descriptor or -1. It does not dereference application memory.
    let duplicated = unsafe {
        libc::fcntl(
            INHERITED_DESCRIPTOR_SLOT,
            libc::F_DUPFD_CLOEXEC,
            INHERITED_DESCRIPTOR_SLOT,
        )
    };
    if duplicated < 0 {
        return Err(DurabilityFailureCode::InheritedDescriptorUnavailable);
    }
    // SAFETY: successful F_DUPFD_CLOEXEC created this process-owned descriptor;
    // transferring that ownership to File is exact and closes it once on drop.
    Ok(unsafe { File::from_raw_fd(duplicated) })
}

fn validate_type(
    metadata: &Metadata,
    expected_directory: bool,
) -> Result<(), DurabilityFailureCode> {
    if metadata.file_type().is_symlink() {
        return Err(DurabilityFailureCode::SymlinkRefused);
    }
    let matches = if expected_directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if matches {
        Ok(())
    } else {
        Err(DurabilityFailureCode::WrongTargetType)
    }
}

#[cfg(target_os = "macos")]
fn persist_descriptor(file: &File, expected_directory: bool) -> Result<(), DurabilityFailureCode> {
    // SAFETY: the descriptor is owned by `file`, remains live for this call, and
    // F_FULLFSYNC takes no pointer argument. The return value is checked before use.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
    if result == 0 {
        return Ok(());
    }
    Err(classify_barrier_failure(
        expected_directory,
        &io::Error::last_os_error(),
    ))
}

#[cfg(target_os = "linux")]
fn persist_descriptor(file: &File, expected_directory: bool) -> Result<(), DurabilityFailureCode> {
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(error) => Err(classify_barrier_failure(expected_directory, &error)),
    }
}

fn classify_barrier_failure(expected_directory: bool, error: &io::Error) -> DurabilityFailureCode {
    classify_barrier_errno(expected_directory, error.raw_os_error())
}

fn classify_barrier_errno(
    expected_directory: bool,
    raw_error: Option<i32>,
) -> DurabilityFailureCode {
    if expected_directory && is_unsupported_directory_errno(raw_error) {
        DurabilityFailureCode::DirectoryPersistenceUnsupported
    } else {
        DurabilityFailureCode::DurabilityBarrierFailed
    }
}

fn is_unsupported_directory_errno(raw_error: Option<i32>) -> bool {
    // Darwin F_FULLFSYNC and Linux fsync use EINVAL or ENOTSUP when the
    // descriptor's filesystem cannot provide the requested directory barrier.
    raw_error == Some(libc::EINVAL) || raw_error == Some(libc::ENOTSUP)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn persist_descriptor(
    _file: &File,
    _expected_directory: bool,
) -> Result<(), DurabilityFailureCode> {
    Err(DurabilityFailureCode::PlatformUnsupported)
}

#[cfg(test)]
mod tests {
    use super::{
        DurabilityFailureCode, DurabilityOperation, Invocation, PlatformContract,
        classify_barrier_errno,
    };
    use std::ffi::OsString;

    #[test]
    fn invocation_requires_the_inherited_descriptor_shape() {
        let output = Invocation::parse([OsString::from("sync-inherited-fd")]).execute();
        assert_eq!(
            output.failure_code,
            Some(DurabilityFailureCode::InvalidInvocation)
        );
    }

    #[test]
    fn platform_contract_matches_build_target() {
        if cfg!(target_os = "macos") {
            assert_eq!(
                PlatformContract::current(),
                PlatformContract::DarwinFullFsyncV1
            );
        } else if cfg!(target_os = "linux") {
            assert_eq!(PlatformContract::current(), PlatformContract::LinuxFsyncV1);
        } else {
            assert_eq!(PlatformContract::current(), PlatformContract::UnsupportedV1);
        }
    }

    #[test]
    fn invalid_operation_is_never_reflected() {
        let output = Invocation::parse([
            OsString::from("private-operation-value"),
            OsString::from("/private/value"),
        ])
        .execute();
        assert_eq!(output.operation, DurabilityOperation::Invalid);
    }

    #[test]
    fn unsupported_directory_barrier_is_typed_separately() {
        assert_eq!(
            classify_barrier_errno(true, Some(libc::EINVAL)),
            DurabilityFailureCode::DirectoryPersistenceUnsupported
        );
        assert_eq!(
            classify_barrier_errno(true, Some(libc::ENOTSUP)),
            DurabilityFailureCode::DirectoryPersistenceUnsupported
        );
    }

    #[test]
    fn directory_io_failure_remains_a_barrier_failure() {
        assert_eq!(
            classify_barrier_errno(true, Some(libc::EIO)),
            DurabilityFailureCode::DurabilityBarrierFailed
        );
        assert_eq!(
            classify_barrier_errno(true, Some(libc::ENOSPC)),
            DurabilityFailureCode::DurabilityBarrierFailed
        );
    }

    #[test]
    fn file_failure_is_never_mislabeled_as_directory_unsupported() {
        assert_eq!(
            classify_barrier_errno(false, Some(libc::EINVAL)),
            DurabilityFailureCode::DurabilityBarrierFailed
        );
    }
}
