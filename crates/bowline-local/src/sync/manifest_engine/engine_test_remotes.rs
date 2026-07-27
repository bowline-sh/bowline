//! Transport doubles for the manifest-sync engine: an in-memory object store +
//! CAS ref, its event-free twin, and a disk-backed store two processes share.
//!
//! Split from the harness module at the doubles/harness seam so neither grows
//! past the source-length gate. The harness owns engines and clocks; this owns
//! everything that stands in for the hosted object store and the CAS ref.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use super::endpoint::NameFolding;
use super::engine_test_support::{fetch_test_tree, publish_test_tree};
use super::manifest::DecodeLimits;
use super::manifest::{
    BlobKey, FileMode, Manifest, ManifestEntry, ManifestKey, WorkspaceCrypto, WorkspacePath,
    physical_blob_key, seal_file,
};
use super::push::{
    BlobReaderUpload, BlobUpload, CasOutcome, ManifestUpload, RefObservation, RemoteObjects,
    RemoteRef, TransportError,
};

/// One recorded transport event, so tests can assert ordering (a blob's metadata
/// commit always precedes the manifest that references it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    PutBlob(String),
    PutBlobReader(String),
    GetBlob(String),
    PutManifest(String),
    GetManifest(String),
    Cas,
}

/// Injected CAS behavior for one push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CasMode {
    #[default]
    Normal,
    /// Mutate the ref but drop the ack (the `AckAmbiguous` path).
    AmbiguousAfterSwap,
    /// Fail the CAS transport before any swap (crash-before-CAS).
    FailBeforeSwap,
}

/// In-memory object store + CAS ref implementing both engine transport traits.
pub(crate) struct FakeRemote {
    blobs: RefCell<BTreeMap<String, Vec<u8>>>,
    manifests: RefCell<BTreeMap<String, Vec<u8>>>,
    reference: RefCell<Option<RefObservation>>,
    version: RefCell<u64>,
    events: RefCell<Vec<Event>>,
    cas_mode: RefCell<CasMode>,
    read_ref_count: Cell<u64>,
    /// When set, every transport call fails — the offline condition the driver's
    /// backoff loop is tested against.
    offline: Cell<bool>,
}

impl FakeRemote {
    pub(crate) fn new() -> Self {
        Self {
            blobs: RefCell::new(BTreeMap::new()),
            manifests: RefCell::new(BTreeMap::new()),
            reference: RefCell::new(None),
            version: RefCell::new(0),
            events: RefCell::new(Vec::new()),
            cas_mode: RefCell::new(CasMode::Normal),
            read_ref_count: Cell::new(0),
            offline: Cell::new(false),
        }
    }

    pub(crate) fn set_cas_mode(&self, mode: CasMode) {
        *self.cas_mode.borrow_mut() = mode;
    }

    /// Drive the network up (`false`) or down (`true`) for the backoff tests.
    pub(crate) fn set_offline(&self, offline: bool) {
        self.offline.set(offline);
    }

    /// Override the OBSERVED head (`read_ref`) without touching the CAS version
    /// counter, so a test can simulate a hosted rollback to a lower version or a
    /// forged high-version ref while the real CAS sequence continues from its own
    /// counter. Distinct from `publish_*`, which always advance monotonically.
    pub(crate) fn force_ref(&self, version: u64, manifest_key: ManifestKey) {
        *self.reference.borrow_mut() = Some(RefObservation {
            version,
            manifest_key,
        });
    }

    fn guard(&self, operation: &'static str) -> Result<(), TransportError> {
        if self.offline.get() {
            return Err(TransportError::new(operation, "simulated offline"));
        }
        Ok(())
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.events.borrow().clone()
    }

    pub(crate) fn blob_put_count(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, Event::PutBlob(_) | Event::PutBlobReader(_)))
            .count()
    }

    /// How many manifest tree nodes were PUT, and how many sealed bytes they
    /// carried. The publish-cost meter the scale fixture reads.
    pub(crate) fn manifest_put_totals(&self) -> (u64, u64) {
        let manifests = self.manifests.borrow();
        self.events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                Event::PutManifest(key) => Some(key),
                _ => None,
            })
            .fold((0, 0), |(count, bytes), key| {
                let sealed = manifests.get(key).map_or(0, |node| node.len() as u64);
                (count + 1, bytes + sealed)
            })
    }

    pub(crate) fn reader_put_count(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, Event::PutBlobReader(_)))
            .count()
    }

    pub(crate) fn current_ref(&self) -> Option<RefObservation> {
        self.reference.borrow().clone()
    }

    pub(crate) fn read_ref_count(&self) -> u64 {
        self.read_ref_count.get()
    }

    /// Decode the manifest the head currently points at, for asserting on the
    /// exact entries a push produced (e.g. that a mode-only change preserved a
    /// file's content identity).
    pub(crate) fn decoded_manifest(&self, crypto: &WorkspaceCrypto) -> Option<Manifest> {
        let key = self.current_ref()?.manifest_key;
        Some(self.decode_manifest(crypto, &key))
    }

    /// Flatten a stored manifest tree by root key, without recording transport
    /// events — an assertion about published state is not itself sync traffic.
    pub(crate) fn decode_manifest(&self, crypto: &WorkspaceCrypto, root: &ManifestKey) -> Manifest {
        fetch_test_tree(
            &SilentObjects(self),
            crypto,
            root,
            &DecodeLimits::default(),
            NameFolding::EXACT,
        )
        .expect("decode current manifest")
        .decoded
        .manifest
    }

    /// A fresh remote holding a snapshot of this one's objects + head, so a peer
    /// `Harness` (which owns its own remote) can pull against the same state.
    pub(crate) fn clone_state(&self) -> FakeRemote {
        FakeRemote {
            blobs: RefCell::new(self.blobs.borrow().clone()),
            manifests: RefCell::new(self.manifests.borrow().clone()),
            reference: RefCell::new(self.reference.borrow().clone()),
            version: RefCell::new(*self.version.borrow()),
            events: RefCell::new(Vec::new()),
            cas_mode: RefCell::new(CasMode::Normal),
            read_ref_count: Cell::new(0),
            offline: Cell::new(false),
        }
    }

    /// Publish a remote manifest directly (a simulated peer), advancing the ref.
    pub(crate) fn publish_manifest(
        &self,
        crypto: &WorkspaceCrypto,
        manifest: &Manifest,
    ) -> ManifestKey {
        let key = publish_test_tree(&SilentObjects(self), crypto, manifest);
        let mut version = self.version.borrow_mut();
        *version += 1;
        *self.reference.borrow_mut() = Some(RefObservation {
            version: *version,
            manifest_key: key.clone(),
        });
        key
    }

    /// Seal + store a file blob so a published manifest can reference it.
    pub(crate) fn publish_blob(&self, crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
        let content_id = crypto.content_id(plaintext);
        let sealed = seal_file(crypto, &content_id, plaintext).expect("seal file");
        let key = physical_blob_key(sealed.as_bytes());
        self.blobs
            .borrow_mut()
            .insert(key.as_str().to_string(), sealed.into_bytes());
        ManifestEntry::File {
            size: plaintext.len() as u64,
            mode: FileMode::new(0o644),
            content_id,
            blob_key: key,
            key_epoch: crypto.key_epoch(),
        }
    }
}

/// The same in-memory object store, minus the event log.
///
/// A simulated peer's publish is remote state that simply exists; recording it
/// as this device's transport traffic would corrupt every ordering and
/// cost-proportionality assertion made against [`FakeRemote::events`].
pub(crate) struct SilentObjects<'a>(pub(crate) &'a FakeRemote);

impl RemoteObjects for SilentObjects<'_> {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.0
            .blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let bytes = fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))?;
        self.0
            .blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), bytes);
        Ok(())
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.0
            .manifests
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.0
            .blobs
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_blob", "missing blob"))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.0
            .manifests
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_manifest", "missing manifest"))
    }
}

impl RemoteObjects for FakeRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_blob")?;
        self.events
            .borrow_mut()
            .push(Event::PutBlob(upload.key.as_str().to_string()));
        self.blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_blob_reader")?;
        self.events
            .borrow_mut()
            .push(Event::PutBlobReader(upload.key.as_str().to_string()));
        let bytes = fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))?;
        self.blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), bytes);
        Ok(())
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_manifest")?;
        self.events
            .borrow_mut()
            .push(Event::PutManifest(upload.key.as_str().to_string()));
        self.manifests
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.guard("get_blob")?;
        self.events
            .borrow_mut()
            .push(Event::GetBlob(key.as_str().to_string()));
        self.blobs
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_blob", "missing blob"))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.guard("get_manifest")?;
        self.events
            .borrow_mut()
            .push(Event::GetManifest(key.as_str().to_string()));
        self.manifests
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_manifest", "missing manifest"))
    }
}

impl RemoteRef for FakeRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        self.guard("read_ref")?;
        self.read_ref_count
            .set(self.read_ref_count.get().saturating_add(1));
        Ok(self.reference.borrow().clone())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        self.guard("compare_and_swap")?;
        self.events.borrow_mut().push(Event::Cas);
        let mode = *self.cas_mode.borrow();
        if mode == CasMode::FailBeforeSwap {
            return Err(TransportError::new("cas", "simulated crash before swap"));
        }
        let current = self.reference.borrow().clone();
        let current_version = current.as_ref().map(|observed| observed.version);
        if current_version != expected_version {
            return Ok(CasOutcome::Lost(
                current.expect("lost implies a current ref"),
            ));
        }
        let mut version = self.version.borrow_mut();
        *version += 1;
        let observed = RefObservation {
            version: *version,
            manifest_key: new_manifest_key.clone(),
        };
        *self.reference.borrow_mut() = Some(observed.clone());
        match mode {
            CasMode::AmbiguousAfterSwap => Ok(CasOutcome::Ambiguous),
            _ => Ok(CasOutcome::Advanced(observed)),
        }
    }
}

/// A disk-backed object store + CAS ref. Unlike [`FakeRemote`] (in-memory), this
/// persists to a directory so a parent test process and a re-invoked child
/// process (the kill-9 matrix, Step 6) share the SAME sealed bytes and CAS head:
/// the physical keys `blake3(sealed)` match across processes only because both
/// read identical persisted blobs.
pub(crate) struct SharedRemote {
    root: PathBuf,
}

impl SharedRemote {
    pub(crate) fn open(root: PathBuf) -> Self {
        fs::create_dir_all(root.join("blobs")).expect("blobs dir");
        fs::create_dir_all(root.join("manifests")).expect("manifests dir");
        Self { root }
    }

    fn blob_path(&self, key: &str) -> PathBuf {
        self.root.join("blobs").join(key)
    }

    fn manifest_path(&self, key: &str) -> PathBuf {
        self.root.join("manifests").join(key)
    }

    fn ref_path(&self) -> PathBuf {
        self.root.join("ref.json")
    }

    fn write_ref(&self, observed: &RefObservation) {
        let line = format!("{}\n{}\n", observed.version, observed.manifest_key.as_str());
        fs::write(self.ref_path(), line).expect("write ref");
    }

    pub(crate) fn current_ref(&self) -> Option<RefObservation> {
        let raw = fs::read_to_string(self.ref_path()).ok()?;
        let mut lines = raw.lines();
        let version = lines.next()?.parse().ok()?;
        let manifest_key = ManifestKey::new(lines.next()?.to_string());
        Some(RefObservation {
            version,
            manifest_key,
        })
    }

    /// Seal + persist a file blob so a published manifest can reference it.
    pub(crate) fn publish_blob(&self, crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
        let content_id = crypto.content_id(plaintext);
        let sealed = seal_file(crypto, &content_id, plaintext).expect("seal file");
        let key = physical_blob_key(sealed.as_bytes());
        fs::write(self.blob_path(key.as_str()), sealed.into_bytes()).expect("write blob");
        ManifestEntry::File {
            size: plaintext.len() as u64,
            mode: FileMode::new(0o644),
            content_id,
            blob_key: key,
            key_epoch: crypto.key_epoch(),
        }
    }

    /// Publish a head from `(path, entry)` pairs, advancing the ref.
    pub(crate) fn publish(
        &self,
        crypto: &WorkspaceCrypto,
        entries: &[(&str, ManifestEntry)],
    ) -> ManifestKey {
        let map: BTreeMap<WorkspacePath, ManifestEntry> = entries
            .iter()
            .map(|(path, entry)| (WorkspacePath::new(*path), entry.clone()))
            .collect();
        let manifest = Manifest::new(crypto.key_epoch(), map);
        let key = publish_test_tree(self, crypto, &manifest);
        let version = self
            .current_ref()
            .map(|observed| observed.version)
            .unwrap_or(0)
            + 1;
        self.write_ref(&RefObservation {
            version,
            manifest_key: key.clone(),
        });
        key
    }
}

impl RemoteObjects for SharedRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        fs::write(self.blob_path(upload.key.as_str()), upload.sealed)
            .map_err(|error| TransportError::new("put_blob", error.to_string()))
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let bytes = fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))?;
        fs::write(self.blob_path(upload.key.as_str()), bytes)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        fs::write(self.manifest_path(upload.key.as_str()), upload.sealed)
            .map_err(|error| TransportError::new("put_manifest", error.to_string()))
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        fs::read(self.blob_path(key.as_str()))
            .map_err(|error| TransportError::new("get_blob", error.to_string()))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        fs::read(self.manifest_path(key.as_str()))
            .map_err(|error| TransportError::new("get_manifest", error.to_string()))
    }
}

impl RemoteRef for SharedRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(self.current_ref())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        // The kill matrix drives one child then the parent — never concurrently —
        // so a read-compare-write is sufficient (no cross-process lock needed).
        let current = self.current_ref();
        if current.as_ref().map(|observed| observed.version) != expected_version {
            return Ok(CasOutcome::Lost(
                current.expect("lost implies a current ref"),
            ));
        }
        let version = current.map(|observed| observed.version).unwrap_or(0) + 1;
        let observed = RefObservation {
            version,
            manifest_key: new_manifest_key.clone(),
        };
        self.write_ref(&observed);
        Ok(CasOutcome::Advanced(observed))
    }
}
