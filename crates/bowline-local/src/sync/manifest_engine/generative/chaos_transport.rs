//! A seeded fault-injecting transport: the Trinity analogue's executor.
//!
//! Wraps the in-memory [`FakeRemote`] and, on every object/ref call, rolls a
//! seeded die to decide whether the call fails before its effect, succeeds and
//! then loses its acknowledgement, or (for the CAS) swaps and returns
//! `Ambiguous`. The after-effect shape is the one that matters: it is the real
//! partial failure the engine must survive — the object landed, the caller was
//! told it did not.
//!
//! Every injected fault is recorded so a failing run reports the exact fault
//! schedule alongside its seed.

use std::cell::{Cell, RefCell};
use std::fmt;

use super::super::engine_test_support::{CasMode, FakeRemote};
use super::super::manifest::{BlobKey, Manifest, ManifestKey, WorkspaceCrypto};
use super::super::push::{
    BlobReaderUpload, BlobUpload, CasOutcome, ManifestUpload, RefObservation, RemoteObjects,
    RemoteRef, TransportError,
};
use super::rng::{Rng, Seed};

/// Where a fault was injected. Named per transport call so a failure report
/// says which round trip broke, never a bare index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultSite {
    PutBlob,
    PutBlobReader,
    PutManifest,
    GetBlob,
    GetManifest,
    ReadRef,
    CompareAndSwap,
}

impl FaultSite {
    /// The `&'static str` operation tag the engine's own `TransportError`
    /// carries, so an injected failure is indistinguishable from a real one.
    const fn operation(self) -> &'static str {
        match self {
            Self::PutBlob => "put_blob",
            Self::PutBlobReader => "put_blob_reader",
            Self::PutManifest => "put_manifest",
            Self::GetBlob => "get_blob",
            Self::GetManifest => "get_manifest",
            Self::ReadRef => "read_ref",
            Self::CompareAndSwap => "compare_and_swap",
        }
    }
}

/// How a fault manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultShape {
    /// The call never reached the store.
    BeforeEffect,
    /// The call took effect; the acknowledgement was lost.
    AfterEffect,
    /// The CAS swapped but returned `Ambiguous` instead of the new head.
    AmbiguousSwap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InjectedFault {
    pub(crate) call: u64,
    pub(crate) site: FaultSite,
    pub(crate) shape: FaultShape,
}

impl fmt::Display for InjectedFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "call#{} {:?} {:?}",
            self.call, self.site, self.shape
        )
    }
}

/// Fault probability as an explicit fraction, so a schedule reads as
/// "one call in six" rather than as a float nobody can reproduce exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FaultRate {
    numerator: u32,
    denominator: u32,
}

impl FaultRate {
    /// No faults at all: the quiescing phase every chaos run ends with, where
    /// the engine must be able to finish what the storm interrupted.
    pub(crate) const CALM: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    pub(crate) const fn new(numerator: u32, denominator: u32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }
}

pub(crate) struct ChaosRemote {
    inner: FakeRemote,
    rng: RefCell<Rng>,
    rate: Cell<FaultRate>,
    calls: Cell<u64>,
    injected: RefCell<Vec<InjectedFault>>,
}

impl ChaosRemote {
    pub(crate) fn new(seed: Seed, rate: FaultRate) -> Self {
        Self {
            inner: FakeRemote::new(),
            rng: RefCell::new(Rng::from_seed(seed)),
            rate: Cell::new(rate),
            calls: Cell::new(0),
            injected: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn set_rate(&self, rate: FaultRate) {
        self.rate.set(rate);
    }

    pub(crate) fn injected(&self) -> Vec<InjectedFault> {
        self.injected.borrow().clone()
    }

    pub(crate) fn decoded_manifest(&self, crypto: &WorkspaceCrypto) -> Option<Manifest> {
        self.inner.decoded_manifest(crypto)
    }

    /// Roll for a fault at `site`. `effectful` sites can also fail *after* the
    /// effect lands; read-only sites cannot (a lost read ack is just a failed
    /// read).
    fn roll(&self, site: FaultSite, effectful: bool) -> Option<FaultShape> {
        let call = self.calls.get().saturating_add(1);
        self.calls.set(call);
        let rate = self.rate.get();
        let mut rng = self.rng.borrow_mut();
        if !rng.chance(rate.numerator, rate.denominator) {
            return None;
        }
        let shape = match site {
            FaultSite::CompareAndSwap if rng.chance(1, 2) => FaultShape::AmbiguousSwap,
            _ if effectful && rng.chance(1, 2) => FaultShape::AfterEffect,
            _ => FaultShape::BeforeEffect,
        };
        drop(rng);
        self.injected
            .borrow_mut()
            .push(InjectedFault { call, site, shape });
        Some(shape)
    }

    fn failure(site: FaultSite, shape: FaultShape) -> TransportError {
        let detail = match shape {
            FaultShape::BeforeEffect => "chaos: dropped before effect",
            FaultShape::AfterEffect => "chaos: acknowledgement lost after effect",
            FaultShape::AmbiguousSwap => "chaos: ambiguous swap",
        };
        TransportError::new(site.operation(), detail)
    }
}

/// Run `effect` under the fault decision for an effectful call.
fn with_effectful_fault<T>(
    fault: Option<FaultShape>,
    site: FaultSite,
    effect: impl FnOnce() -> Result<T, TransportError>,
) -> Result<T, TransportError> {
    match fault {
        Some(FaultShape::BeforeEffect) => Err(ChaosRemote::failure(site, FaultShape::BeforeEffect)),
        Some(FaultShape::AfterEffect) => {
            effect()?;
            Err(ChaosRemote::failure(site, FaultShape::AfterEffect))
        }
        // A CAS-only shape cannot reach an object call; treat it as a plain
        // pre-effect drop rather than inventing a fourth behaviour.
        Some(FaultShape::AmbiguousSwap) => {
            Err(ChaosRemote::failure(site, FaultShape::BeforeEffect))
        }
        None => effect(),
    }
}

impl RemoteObjects for ChaosRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        let fault = self.roll(FaultSite::PutBlob, true);
        with_effectful_fault(fault, FaultSite::PutBlob, || self.inner.put_blob(upload))
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let fault = self.roll(FaultSite::PutBlobReader, true);
        with_effectful_fault(fault, FaultSite::PutBlobReader, || {
            self.inner.put_blob_reader(upload)
        })
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        let fault = self.roll(FaultSite::PutManifest, true);
        with_effectful_fault(fault, FaultSite::PutManifest, || {
            self.inner.put_manifest(upload)
        })
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        match self.roll(FaultSite::GetBlob, false) {
            Some(shape) => Err(Self::failure(FaultSite::GetBlob, shape)),
            None => self.inner.get_blob(key),
        }
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        match self.roll(FaultSite::GetManifest, false) {
            Some(shape) => Err(Self::failure(FaultSite::GetManifest, shape)),
            None => self.inner.get_manifest(key),
        }
    }
}

impl RemoteRef for ChaosRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        match self.roll(FaultSite::ReadRef, false) {
            Some(shape) => Err(Self::failure(FaultSite::ReadRef, shape)),
            None => self.inner.read_ref(),
        }
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        match self.roll(FaultSite::CompareAndSwap, true) {
            Some(FaultShape::BeforeEffect) => Err(Self::failure(
                FaultSite::CompareAndSwap,
                FaultShape::BeforeEffect,
            )),
            Some(FaultShape::AmbiguousSwap | FaultShape::AfterEffect) => {
                // Both shapes are the same physical event on a CAS — the swap
                // committed and the caller cannot tell. Drive it through the
                // fake's own ambiguous mode so the engine sees the real
                // `CasOutcome::Ambiguous` resolution path, not a transport error.
                self.inner.set_cas_mode(CasMode::AmbiguousAfterSwap);
                let outcome = self
                    .inner
                    .compare_and_swap(expected_version, new_manifest_key);
                self.inner.set_cas_mode(CasMode::Normal);
                outcome
            }
            None => self
                .inner
                .compare_and_swap(expected_version, new_manifest_key),
        }
    }
}
