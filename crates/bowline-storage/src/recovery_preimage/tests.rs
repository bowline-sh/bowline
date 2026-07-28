use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use bowline_core::ids::WorkspaceId;

use crate::{EnvelopeError, StorageKey};

use super::*;

#[test]
fn env_preimage_is_only_retained_as_mode_restricted_ciphertext() {
    let temp = TempDir::new("env-secrecy");
    let (plaintext_locator, sealed_locator) = locators("epoch-secret", "project/.env");
    let plaintext_path = temp.path().join(plaintext_locator.as_path());
    fs::create_dir_all(plaintext_path.parent().expect("plaintext parent"))
        .expect("create plaintext parent");
    let secret = b"DATABASE_URL=postgres://private-token@example.test/db";
    fs::write(&plaintext_path, secret).expect("write plaintext recovery file");
    let context = context(
        "ws-secret",
        "epoch-secret",
        "project/.env",
        "content-env",
        7,
    );
    assert!(!format!("{context:?}").contains(".env"));

    let sealed = seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(7),
        context: &context,
    })
    .expect("seal preimage");

    assert_eq!(sealed.locator(), &sealed_locator);
    assert_eq!(sealed.key_epoch().value(), 7);
    assert!(
        !plaintext_path.exists(),
        "plaintext artifact must be deleted"
    );
    assert!(!sealed.locator().as_str().contains(".env"));
    let encrypted_path = temp.path().join(sealed.locator().as_path());
    let encrypted = fs::read(&encrypted_path).expect("read encrypted artifact");
    assert!(!contains_bytes(&encrypted, secret));
    assert_eq!(
        open(
            temp.path(),
            sealed.locator(),
            StorageKey::deterministic(7),
            &context
        ),
        secret
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&encrypted_path)
                .expect("encrypted metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut current = encrypted_path.parent();
        while let Some(directory) = current.filter(|path| path.starts_with(temp.path())) {
            assert_eq!(
                fs::metadata(directory)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            if directory == temp.path() {
                break;
            }
            current = directory.parent();
        }
    }
}

#[test]
fn plaintext_and_encrypted_state_can_live_under_distinct_roots() {
    let plaintext_root = TempDir::new("split-root-plaintext");
    let sealed_state_root = TempDir::new("split-root-encrypted");
    let plaintext_locator = seed_plaintext(
        plaintext_root.path(),
        "epoch",
        "src/config",
        b"workspace-local preimage",
    );
    let sealed_locator = sealed_locator("epoch", "src/config");
    let context = context("ws", "epoch", "src/config", "content", 1);

    seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
        plaintext_root: plaintext_root.path(),
        sealed_state_root: sealed_state_root.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    })
    .expect("seal across roots");

    assert!(
        !plaintext_root
            .path()
            .join(plaintext_locator.as_path())
            .exists()
    );
    assert!(
        !plaintext_root
            .path()
            .join(sealed_locator.as_path())
            .exists()
    );
    assert_eq!(
        open(
            sealed_state_root.path(),
            &sealed_locator,
            StorageKey::deterministic(1),
            &context,
        ),
        b"workspace-local preimage"
    );
}

#[test]
fn opening_rejects_wrong_key_and_every_bound_context_identity() {
    let temp = TempDir::new("context-rejection");
    seed_plaintext(temp.path(), "epoch-a", "src/a.txt", b"recover me");
    let sealed_locator = sealed_locator("epoch-a", "src/a.txt");
    let sealed_context = context("ws-a", "epoch-a", "src/a.txt", "content-a", 3);
    seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(3),
        context: &sealed_context,
    })
    .expect("seal preimage");

    assert_open_rejected(open_result(
        temp.path(),
        &sealed_locator,
        StorageKey::deterministic(4),
        &sealed_context,
    ));
    for wrong_context in [
        context("ws-b", "epoch-a", "src/a.txt", "content-a", 3),
        context("ws-a", "epoch-b", "src/a.txt", "content-a", 3),
        context("ws-a", "epoch-a", "src/b.txt", "content-a", 3),
        context("ws-a", "epoch-a", "src/a.txt", "content-b", 3),
        context("ws-a", "epoch-a", "src/a.txt", "content-a", 4),
    ] {
        assert_open_rejected(open_result(
            temp.path(),
            &sealed_locator,
            StorageKey::deterministic(3),
            &wrong_context,
        ));
    }

    let moved_locator = locators("epoch-moved", "src/moved.txt").1;
    let moved_path = temp.path().join(moved_locator.as_path());
    fs::create_dir_all(moved_path.parent().expect("moved parent")).expect("create moved parent");
    fs::copy(temp.path().join(sealed_locator.as_path()), &moved_path).expect("copy envelope");
    assert_open_rejected(open_result(
        temp.path(),
        &moved_locator,
        StorageKey::deterministic(3),
        &sealed_context,
    ));
}

#[test]
fn locators_reject_absolute_traversal_and_noncanonical_paths() {
    for invalid in [
        "",
        "/absolute/recovery",
        "../outside",
        "inside/../outside",
        "inside/./artifact",
        "inside//artifact",
        "inside\\artifact",
    ] {
        assert!(matches!(
            LocalRecoveryPreimageLocator::new(invalid),
            Err(LocalRecoveryPreimageError::InvalidLocator { .. })
        ));
    }
    let valid = sealed_locator("epoch", "inside/artifact");
    assert_eq!(
        LocalRecoveryPreimageLocator::new(valid.as_str()).expect("persisted locator"),
        valid
    );
    let workspace_path = LocalRecoveryWorkspacePath::new("../outside");
    assert!(matches!(
        workspace_path,
        Err(LocalRecoveryPreimageError::InvalidLocator { .. })
    ));
    assert!(matches!(
        LocalRecoveryPreimageLocator::new("filesystem-epochs/encrypted-quarantine/not-hashes"),
        Err(LocalRecoveryPreimageError::InvalidLocator { .. })
    ));
    assert!(LocalRecoveryKeyEpoch::new(0).is_err());
}

#[test]
fn failed_atomic_publication_removes_temp_and_retains_plaintext() {
    let temp = TempDir::new("atomic-cleanup");
    let plaintext_locator = seed_plaintext(
        temp.path(),
        "epoch",
        "src/a.txt",
        b"must remain recoverable",
    );
    let sealed_locator = sealed_locator("epoch", "src/a.txt");
    let sealed_path = temp.path().join(sealed_locator.as_path());
    fs::create_dir_all(&sealed_path).expect("create destination directory collision");
    let context = context("ws", "epoch", "src/a.txt", "content", 1);

    let result = seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    });

    assert!(matches!(result, Err(LocalRecoveryPreimageError::Io { .. })));
    assert_eq!(
        fs::read(temp.path().join(plaintext_locator.as_path())).expect("plaintext retained"),
        b"must remain recoverable"
    );
    let parent = sealed_path.parent().expect("sealed parent");
    assert!(
        fs::read_dir(parent)
            .expect("read sealed parent")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .contains("bowline-tmp"))
    );
}

#[test]
fn existing_authenticated_envelope_finishes_plaintext_cleanup_idempotently() {
    let temp = TempDir::new("idempotent-resume");
    let plaintext = b"recovery bytes committed before crash";
    let plaintext_locator = seed_plaintext(temp.path(), "epoch", "src/a.txt", plaintext);
    let context = context("ws", "epoch", "src/a.txt", "content", 1);
    let request = || SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    };
    let first = seal_local_recovery_preimage(request()).expect("initial seal");
    let plaintext_path = temp.path().join(plaintext_locator.as_path());
    fs::write(&plaintext_path, plaintext).expect("simulate crash-retained plaintext");

    let resumed = seal_local_recovery_preimage(request()).expect("idempotent resume");

    assert_eq!(resumed, first);
    assert!(!plaintext_path.exists());
    assert_eq!(
        open(
            temp.path(),
            first.locator(),
            StorageKey::deterministic(1),
            &context,
        ),
        plaintext
    );
}

#[test]
fn authenticated_envelope_resumes_after_plaintext_unlink_before_receipt() {
    let temp = TempDir::new("resume-after-unlink");
    let plaintext_locator = seed_plaintext(temp.path(), "epoch", "src/a.txt", b"recoverable");
    let context = context("ws", "epoch", "src/a.txt", "content", 1);
    let request = || SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    };
    let first = seal_local_recovery_preimage(request()).expect("initial seal");
    assert!(!temp.path().join(plaintext_locator.as_path()).exists());

    let resumed = seal_local_recovery_preimage(request()).expect("resume without plaintext");

    assert_eq!(resumed, first);
}

#[test]
fn authenticated_envelope_resumes_after_plaintext_root_disappears() {
    let temp = TempDir::new("resume-after-plaintext-root");
    let plaintext_root = temp.path().join("plaintext");
    let sealed_root = temp.path().join("sealed");
    fs::create_dir(&plaintext_root).expect("plaintext root");
    fs::create_dir(&sealed_root).expect("sealed root");
    seed_plaintext(&plaintext_root, "epoch", "src/a.txt", b"recoverable");
    let context = context("ws", "epoch", "src/a.txt", "content", 1);
    let request = || SealLocalRecoveryPreimageRequest {
        plaintext_root: &plaintext_root,
        sealed_state_root: &sealed_root,
        key: StorageKey::deterministic(1),
        context: &context,
    };
    let first = seal_local_recovery_preimage(request()).expect("initial seal");
    fs::remove_dir_all(&plaintext_root).expect("plaintext root disappears");

    let resumed =
        seal_local_recovery_preimage(request()).expect("sealed envelope is sufficient to resume");

    assert_eq!(resumed, first);
    assert_eq!(
        open(
            &sealed_root,
            first.locator(),
            StorageKey::deterministic(1),
            &context,
        ),
        b"recoverable"
    );
}

#[test]
fn absent_plaintext_never_accepts_tampered_envelope() {
    let temp = TempDir::new("resume-tamper");
    seed_plaintext(temp.path(), "epoch", "src/a.txt", b"recoverable");
    let context = context("ws", "epoch", "src/a.txt", "content", 1);
    let request = || SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    };
    let sealed = seal_local_recovery_preimage(request()).expect("initial seal");
    let sealed_path = temp.path().join(sealed.locator().as_path());
    let mut bytes = fs::read(&sealed_path).expect("read envelope");
    let last = bytes.last_mut().expect("ciphertext exists");
    *last ^= 1;
    fs::write(sealed_path, bytes).expect("tamper envelope");

    assert!(matches!(
        seal_local_recovery_preimage(request()),
        Err(LocalRecoveryPreimageError::Envelope(
            EnvelopeError::VerificationFailed
        ))
    ));
}

#[test]
fn existing_envelope_never_deletes_mismatched_plaintext() {
    let temp = TempDir::new("idempotent-mismatch");
    let plaintext_locator = seed_plaintext(temp.path(), "epoch", "src/a.txt", b"original");
    let context = context("ws", "epoch", "src/a.txt", "content", 1);
    let request = || SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &context,
    };
    seal_local_recovery_preimage(request()).expect("initial seal");
    let plaintext_path = temp.path().join(plaintext_locator.as_path());
    fs::write(&plaintext_path, b"newer user bytes").expect("new plaintext transition");

    assert!(matches!(
        seal_local_recovery_preimage(request()),
        Err(LocalRecoveryPreimageError::ExistingPreimageMismatch)
    ));
    assert_eq!(
        fs::read(plaintext_path).expect("mismatched plaintext retained"),
        b"newer user bytes"
    );
}

#[test]
fn source_change_rolls_back_new_envelope_and_allows_replan() {
    let temp = TempDir::new("source-revalidation-rollback");
    let plaintext_locator = seed_plaintext(temp.path(), "epoch", "src/a.txt", b"original");
    let original_context = context("ws", "epoch", "src/a.txt", "content-original", 1);
    let original_result = seal_local_recovery_preimage_with(
        SealLocalRecoveryPreimageRequest {
            plaintext_root: temp.path(),
            sealed_state_root: temp.path(),
            key: StorageKey::deterministic(1),
            context: &original_context,
        },
        |plaintext_path| {
            fs::write(plaintext_path, b"newer user bytes")?;
            Ok(())
        },
    );

    let Err(LocalRecoveryPreimageError::PlaintextRevalidation { source, .. }) = original_result
    else {
        panic!("source change must fail revalidation");
    };
    assert!(matches!(
        *source,
        LocalRecoveryPreimageError::PlaintextChanged
    ));
    assert_eq!(
        fs::read(temp.path().join(plaintext_locator.as_path())).expect("new plaintext retained"),
        b"newer user bytes"
    );
    assert!(
        !temp
            .path()
            .join(original_context.sealed_locator().as_path())
            .exists()
    );

    let replanned_context = context("ws", "epoch", "src/a.txt", "content-newer", 1);
    let sealed = seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
        plaintext_root: temp.path(),
        sealed_state_root: temp.path(),
        key: StorageKey::deterministic(1),
        context: &replanned_context,
    })
    .expect("replanned seal");
    assert_eq!(
        open(
            temp.path(),
            sealed.locator(),
            StorageKey::deterministic(1),
            &replanned_context,
        ),
        b"newer user bytes"
    );
}

#[cfg(unix)]
#[test]
fn symlink_state_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlink-rejection");
    let outside = TempDir::new("symlink-outside");
    let linked_state = temp.path().join("linked-state");
    symlink(outside.path(), &linked_state).expect("state root symlink");
    let (source, _) = locators("epoch", "secret");
    let outside_source = outside.path().join(source.as_path());
    fs::create_dir_all(outside_source.parent().expect("outside source parent"))
        .expect("outside source parent");
    fs::write(&outside_source, b"secret").expect("outside source");
    let context = context("ws", "epoch", "secret", "content", 1);

    assert!(matches!(
        seal_local_recovery_preimage(SealLocalRecoveryPreimageRequest {
            plaintext_root: &linked_state,
            sealed_state_root: temp.path(),
            key: StorageKey::deterministic(1),
            context: &context,
        }),
        Err(LocalRecoveryPreimageError::UnsafeDirectory { .. })
    ));
    assert_eq!(
        fs::read(outside_source).expect("outside source intact"),
        b"secret"
    );
}

fn seed_plaintext(
    root: &Path,
    epoch: &str,
    path: &str,
    bytes: &[u8],
) -> LocalRecoveryPlaintextLocator {
    let (locator, _) = locators(epoch, path);
    let path = root.join(locator.as_path());
    fs::create_dir_all(path.parent().expect("plaintext parent")).expect("create parent");
    fs::write(path, bytes).expect("seed plaintext");
    locator
}

fn locators(
    epoch: &str,
    path: &str,
) -> (LocalRecoveryPlaintextLocator, LocalRecoveryPreimageLocator) {
    let epoch = LocalRecoveryEpochIdentity::new(epoch).expect("epoch identity");
    let path = LocalRecoveryWorkspacePath::new(path).expect("workspace path");
    (
        LocalRecoveryPlaintextLocator::for_epoch_path(&epoch, &path),
        LocalRecoveryPreimageLocator::for_epoch_path(&epoch, &path),
    )
}

fn sealed_locator(epoch: &str, path: &str) -> LocalRecoveryPreimageLocator {
    locators(epoch, path).1
}

fn context(
    workspace_id: &str,
    epoch: &str,
    path: &str,
    preimage: &str,
    key_epoch: u32,
) -> LocalRecoveryPreimageContext {
    let workspace_id = WorkspaceId::new(workspace_id);
    let epoch = LocalRecoveryEpochIdentity::new(epoch).expect("epoch identity");
    let path = LocalRecoveryWorkspacePath::new(path).expect("workspace path");
    let preimage = LocalRecoveryExpectedPreimageIdentity::new(preimage).expect("preimage identity");
    LocalRecoveryPreimageContext::new(
        &workspace_id,
        &epoch,
        &path,
        &preimage,
        LocalRecoveryKeyEpoch::new(key_epoch).expect("key epoch"),
    )
    .expect("valid context")
}

fn open(
    root: &Path,
    locator: &LocalRecoveryPreimageLocator,
    key: StorageKey,
    context: &LocalRecoveryPreimageContext,
) -> Vec<u8> {
    open_result(root, locator, key, context).expect("open preimage")
}

fn open_result(
    root: &Path,
    locator: &LocalRecoveryPreimageLocator,
    key: StorageKey,
    context: &LocalRecoveryPreimageContext,
) -> Result<Vec<u8>, LocalRecoveryPreimageError> {
    open_local_recovery_preimage(OpenLocalRecoveryPreimageRequest {
        sealed_state_root: root,
        sealed_locator: locator,
        key,
        context,
    })
}

fn assert_open_rejected(result: Result<Vec<u8>, LocalRecoveryPreimageError>) {
    assert!(matches!(
        result,
        Err(LocalRecoveryPreimageError::Envelope(
            EnvelopeError::VerificationFailed | EnvelopeError::WrongContext
        )) | Err(LocalRecoveryPreimageError::ContextLocatorMismatch)
    ));
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "bowline-recovery-preimage-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup_attempt = fs::remove_dir_all(&self.path);
    }
}

const PINNED_RECOVERY_ASSOCIATED_DATA: &str = concat!(
    "01000000000000001f626f776c696e652d6c6f63616c2d7265636f766572792d707265696d616765",
    "0000000000000011776f726b73706163652d69642d6861736800000000000000447773685f316166",
    "39346231343263353437616562656337383330666264336333333734303934646666393730323164",
    "336165343035316535336235316261343035306665000000000000001966696c6573797374656d2d",
    "65706f63682d6964656e74697479000000000000000c65706f63682d70696e6e6564000000000000",
    "0017776f726b73706163652d706174682d6964656e74697479000000000000000c70726f6a656374",
    "2f2e656e76000000000000001a65787065637465642d707265696d6167652d6964656e7469747900",
    "00000000000009636f6e74656e742d6100000000000000076c6f6361746f7200000000000000b966",
    "696c6573797374656d2d65706f6368732f656e637279707465642d71756172616e74696e652f6261",
    "30363735353065643365616239363632643337333165623531356339656136653332653933383836",
    "323835343933626666363538393835666230373238612f3634656634616466386665653563356530",
    "31383736383161633830316239663664613736653265383137613238646631303332663031363030",
    "393035393461302e626f776c696e652d656e76656c6f706500000000000000096b65792d65706f63",
    "68000000000000000400000001000000000000000e666f726d61742d76657273696f6e0000000000",
    "0000020001",
);

/// Pins the exact associated-data bytes guarding a sealed recovery preimage.
///
/// These envelopes hold `.env` contents whose plaintext has already been
/// unlinked, so a change to this encoding is unrecoverable secret loss rather
/// than a re-sync. If this fails, revert the encoding change; do not re-bless
/// the literal.
#[test]
fn recovery_preimage_associated_data_bytes_are_pinned() {
    let context = context("ws-pinned", "epoch-pinned", "project/.env", "content-a", 1);
    let associated_data = context
        .associated_data(context.sealed_locator())
        .expect("associated data");

    assert_eq!(hex(&associated_data), PINNED_RECOVERY_ASSOCIATED_DATA);
    assert!(
        !String::from_utf8_lossy(&associated_data).contains('{'),
        "associated data must not be a JSON rendering"
    );
}

#[test]
fn recovery_preimage_associated_data_binds_every_context_field() {
    let baseline = context("ws-a", "epoch-a", "project/.env", "content-a", 1);
    let baseline_bytes = baseline
        .associated_data(baseline.sealed_locator())
        .expect("baseline associated data");

    for variant in [
        context("ws-b", "epoch-a", "project/.env", "content-a", 1),
        context("ws-a", "epoch-b", "project/.env", "content-a", 1),
        context("ws-a", "epoch-a", "project/other.env", "content-a", 1),
        context("ws-a", "epoch-a", "project/.env", "content-b", 1),
        context("ws-a", "epoch-a", "project/.env", "content-a", 2),
    ] {
        assert_ne!(
            baseline_bytes,
            variant
                .associated_data(variant.sealed_locator())
                .expect("variant associated data")
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
