//! Mechanical native-watcher coverage boundaries.
//!
//! A timeout can stop an attempt, but only a platform acknowledgement can
//! create a boundary. Darwin overlaps two FSEvents streams at an exact journal
//! cursor through `HistoryDone`; Linux installs recursive watches and drains
//! callbacks to `WouldBlock` on the inotify event loop.

use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod fallback;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub use darwin::NativeWatcherCoverageAdapter;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use fallback::NativeWatcherCoverageAdapter;
#[cfg(target_os = "linux")]
pub use linux::NativeWatcherCoverageAdapter;

/// Nonblocking data callback tagged with the native stream epoch.
pub type NativeEventHandler =
    Arc<dyn Fn(WatcherStreamEpoch, notify::Result<notify::Event>) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// One native fidelity-loss observation for recovery coordination.
pub struct NativeCoverageObservation {
    stream_epoch: WatcherStreamEpoch,
    loss: WatcherCoverageLoss,
}

impl NativeCoverageObservation {
    /// Return the native stream that observed the loss.
    pub const fn stream_epoch(self) -> WatcherStreamEpoch {
        self.stream_epoch
    }

    /// Return the typed native loss cause.
    pub const fn loss(self) -> WatcherCoverageLoss {
        self.loss
    }
}

#[cfg(test)]
pub(crate) fn test_native_coverage_observation(
    epoch: u64,
    loss: WatcherCoverageLoss,
) -> NativeCoverageObservation {
    NativeCoverageObservation {
        stream_epoch: WatcherStreamEpoch(
            NonZeroU64::new(epoch).expect("test stream epoch must be nonzero"),
        ),
        loss,
    }
}

#[derive(Clone, Debug)]
/// Single-consumer, bounded outward lane for native loss observations.
///
/// A full lane coalesces further observations because one pending loss already
/// requires recovery. Native close authority is invalidated before publication,
/// so coalescing cannot hide a recovery requirement.
pub struct NativeCoverageObservationReceiver {
    receiver: Receiver<NativeCoverageObservation>,
}

impl NativeCoverageObservationReceiver {
    /// Receive the next pending observation without waiting.
    pub fn try_recv(&self) -> Result<NativeCoverageObservation, crossbeam_channel::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Wait for one observation up to the supplied bound.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<NativeCoverageObservation, crossbeam_channel::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[derive(Clone, Debug)]
struct NativeCoverageObservationPublisher {
    sender: Sender<NativeCoverageObservation>,
}

impl NativeCoverageObservationPublisher {
    fn publish(&self, observation: NativeCoverageObservation) {
        match self.sender.try_send(observation) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                // The correctness authority was already invalidated. A missing
                // observer cannot make an old boundary current again.
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable identity for one native watcher stream.
pub struct WatcherStreamEpoch(NonZeroU64);

impl WatcherStreamEpoch {
    /// Return the nonzero integer identity.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    const fn fallback() -> Self {
        Self(NonZeroU64::MIN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable identity for one requested coverage boundary.
pub struct WatcherBoundaryId(NonZeroU64);

impl WatcherBoundaryId {
    /// Return the nonzero integer identity.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Debug)]
/// Process/workspace-lifecycle allocator for native watcher proof identities.
///
/// A clone must be retained across watcher reconstruction. Allocating inside a
/// platform adapter would allow an old boundary to alias a later stream after
/// the adapter was rebuilt.
pub struct WatcherCoverageIds {
    next_epoch: Arc<AtomicU64>,
    next_boundary: Arc<AtomicU64>,
    authority: WatcherCoverageAuthority,
    observations: NativeCoverageObservationPublisher,
    observation_receiver: NativeCoverageObservationReceiver,
}

impl WatcherCoverageIds {
    /// Start a new process/workspace identity domain.
    pub fn new() -> Self {
        let (observation_tx, observation_rx) = crossbeam_channel::bounded(1);
        Self {
            next_epoch: Arc::new(AtomicU64::new(1)),
            next_boundary: Arc::new(AtomicU64::new(1)),
            authority: WatcherCoverageAuthority::new(),
            observations: NativeCoverageObservationPublisher {
                sender: observation_tx,
            },
            observation_receiver: NativeCoverageObservationReceiver {
                receiver: observation_rx,
            },
        }
    }

    /// Return the single outward native-loss lane for this lifecycle domain.
    ///
    /// Consumers must arrange that exactly one receiver clone drains the lane.
    /// Cloning exists so watcher-host reconstruction can retain the same
    /// process/workspace identity domain without transferring ownership through
    /// the callback hot path.
    pub fn observation_receiver(&self) -> NativeCoverageObservationReceiver {
        self.observation_receiver.clone()
    }

    fn next(counter: &AtomicU64) -> Result<NonZeroU64, WatcherCoverageError> {
        let current = counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| WatcherCoverageError::IdentifierExhausted)?;
        NonZeroU64::new(current).ok_or(WatcherCoverageError::IdentifierExhausted)
    }

    pub(super) fn next_epoch(&self) -> Result<WatcherStreamEpoch, WatcherCoverageError> {
        Self::next(&self.next_epoch).map(WatcherStreamEpoch)
    }

    pub(super) fn next_boundary(&self) -> Result<WatcherBoundaryId, WatcherCoverageError> {
        Self::next(&self.next_boundary).map(WatcherBoundaryId)
    }

    pub(super) fn invalidate_current_boundary(&self) {
        self.authority.invalidate();
    }

    pub(super) fn observe_loss(&self, stream_epoch: WatcherStreamEpoch, loss: WatcherCoverageLoss) {
        // This ordering is the protocol: once an outward loss can be observed,
        // no previously issued close guard may still authorize Ready.
        self.invalidate_current_boundary();
        self.observations
            .publish(NativeCoverageObservation { stream_epoch, loss });
    }

    #[cfg(test)]
    pub(super) fn close_guard(
        &self,
        boundary_id: WatcherBoundaryId,
    ) -> Result<WatcherCoverageCloseGuard, WatcherCoverageError> {
        self.authority.close_guard(boundary_id)
    }

    pub(super) fn current_authority_generation(&self) -> u64 {
        self.authority.generation.load(Ordering::Acquire)
    }

    pub(super) fn close_guard_at(
        &self,
        boundary_id: WatcherBoundaryId,
        generation: u64,
    ) -> Result<WatcherCoverageCloseGuard, WatcherCoverageError> {
        self.authority.close_guard_at(boundary_id, generation)
    }
}

impl Default for WatcherCoverageIds {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct WatcherCoverageAuthority {
    generation: Arc<AtomicU64>,
}

impl WatcherCoverageAuthority {
    fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(1)),
        }
    }

    fn invalidate(&self) {
        let _ = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            });
    }

    #[cfg(test)]
    fn close_guard(
        &self,
        boundary_id: WatcherBoundaryId,
    ) -> Result<WatcherCoverageCloseGuard, WatcherCoverageError> {
        let generation = self.generation.load(Ordering::Acquire);
        self.close_guard_at(boundary_id, generation)
    }

    fn close_guard_at(
        &self,
        boundary_id: WatcherBoundaryId,
        generation: u64,
    ) -> Result<WatcherCoverageCloseGuard, WatcherCoverageError> {
        if generation == u64::MAX {
            return Err(WatcherCoverageError::IdentifierExhausted);
        }
        Ok(WatcherCoverageCloseGuard {
            authority: self.clone(),
            token: WatcherCoverageCloseToken {
                boundary_id,
                generation: NonZeroU64::new(generation)
                    .ok_or(WatcherCoverageError::IdentifierExhausted)?,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Host-scoped FSEvents journal cursor.
pub struct FseventsCursor(u64);

impl FseventsCursor {
    /// Return the native FSEvents event identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Evidence that the replacement FSEvents stream emitted `HistoryDone`.
pub struct DarwinHistoryDone;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Monotonic count of completed synchronous native stream flushes.
pub struct DarwinFlushGeneration(NonZeroU64);

impl DarwinFlushGeneration {
    /// Return the native flush generation.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Monotonic count of data callbacks that returned to FSEvents.
pub struct WatcherCallbackGeneration(u64);

impl WatcherCallbackGeneration {
    /// Return the callback-dispatch generation.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Native loss-authority generation captured by a post-scan seal.
pub struct WatcherLossGeneration(NonZeroU64);

impl WatcherLossGeneration {
    /// Return the native loss generation.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// How a Darwin replacement stream establishes coverage for its predecessor.
pub enum DarwinCoverageStart {
    /// Replay from the predecessor's last mechanically safe journal cursor.
    CursorReplay {
        /// Last cursor known not to carry a loss/discontinuity observation.
        covered_last_safe: FseventsCursor,
        /// Exact cursor supplied to the replacement stream.
        replay_from: FseventsCursor,
        /// Loss that required replay, if the predecessor was already degraded.
        recovery_cause: Option<WatcherCoverageLoss>,
    },
    /// Start from a new host cursor because the predecessor cursor is unusable.
    FreshStream {
        /// Host cursor captured before starting the replacement stream.
        fresh_from: FseventsCursor,
        /// Discontinuity covered by the mandatory authoritative full scan.
        discontinuity: WatcherCoverageLoss,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Exact A-to-B FSEvents overlap proof.
pub struct DarwinCoverageBoundary {
    boundary_id: WatcherBoundaryId,
    covered_epoch: WatcherStreamEpoch,
    live_epoch: WatcherStreamEpoch,
    start: DarwinCoverageStart,
    history_through: FseventsCursor,
    history_done: DarwinHistoryDone,
    must_scan_subdirs: bool,
    sealed_through: FseventsCursor,
    flush_generation: DarwinFlushGeneration,
    loss_generation: WatcherLossGeneration,
    callback_generation: WatcherCallbackGeneration,
}

impl DarwinCoverageBoundary {
    /// Return this boundary's identity.
    pub const fn boundary_id(self) -> WatcherBoundaryId {
        self.boundary_id
    }

    /// Return the epoch of the covered A stream.
    pub const fn covered_epoch(self) -> WatcherStreamEpoch {
        self.covered_epoch
    }

    /// Return the epoch of the replacement B stream.
    pub const fn live_epoch(self) -> WatcherStreamEpoch {
        self.live_epoch
    }

    /// Return the replay or discontinuity proof used to start B.
    pub const fn start(self) -> DarwinCoverageStart {
        self.start
    }

    /// Return B's final cursor when `HistoryDone` arrived.
    pub const fn history_through(self) -> FseventsCursor {
        self.history_through
    }

    /// Return the typed `HistoryDone` evidence marker.
    pub const fn history_done(self) -> DarwinHistoryDone {
        self.history_done
    }

    /// Whether replay included a `MustScanSubDirs` observation.
    pub const fn must_scan_subdirs(self) -> bool {
        self.must_scan_subdirs
    }

    /// Return the final delivered cursor captured after synchronous flush.
    pub const fn sealed_through(self) -> FseventsCursor {
        self.sealed_through
    }

    /// Return the synchronous native flush generation.
    pub const fn flush_generation(self) -> DarwinFlushGeneration {
        self.flush_generation
    }

    /// Return the native invalidation generation captured by the seal.
    pub const fn loss_generation(self) -> WatcherLossGeneration {
        self.loss_generation
    }

    /// Return the callback-return generation captured after the flush.
    pub const fn callback_generation(self) -> WatcherCallbackGeneration {
        self.callback_generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Darwin live-stream preparation retained across the authoritative scan.
pub struct DarwinCoveragePreparation {
    covered_epoch: WatcherStreamEpoch,
    live_epoch: WatcherStreamEpoch,
    start: DarwinCoverageStart,
    history_through: FseventsCursor,
    must_scan_subdirs: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Linux watcher-thread preparation retained across the authoritative scan.
pub struct LinuxCoveragePreparation {
    stream_epoch: WatcherStreamEpoch,
    watcher_ready: LinuxWatcherReady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Platform live-stream preparation which cannot itself authorize closure.
pub enum WatcherCoveragePreparation {
    /// A replacement FSEvents stream is live and replay completed.
    Darwin(DarwinCoveragePreparation),
    /// The inotify watcher graph is live.
    Linux(LinuxCoveragePreparation),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Evidence that Linux recursively installed the initial watcher graph.
pub struct LinuxWatcherReady {
    control_id: WatcherBoundaryId,
}

impl LinuxWatcherReady {
    /// Return the event-loop control identity that produced this marker.
    pub const fn control_id(self) -> WatcherBoundaryId {
        self.control_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Evidence that Linux drained callbacks after the final watch mutation.
pub struct LinuxCallbackDrain {
    control_id: WatcherBoundaryId,
}

impl LinuxCallbackDrain {
    /// Return the event-loop control identity that produced this marker.
    pub const fn control_id(self) -> WatcherBoundaryId {
        self.control_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Same-event-loop Linux watcher-ready and callback-drain proof.
pub struct LinuxCoverageBoundary {
    boundary_id: WatcherBoundaryId,
    stream_epoch: WatcherStreamEpoch,
    watcher_ready: LinuxWatcherReady,
    callback_drain: LinuxCallbackDrain,
}

impl LinuxCoverageBoundary {
    /// Return this boundary's identity.
    pub const fn boundary_id(self) -> WatcherBoundaryId {
        self.boundary_id
    }

    /// Return the inotify worker epoch that owns both markers.
    pub const fn stream_epoch(self) -> WatcherStreamEpoch {
        self.stream_epoch
    }

    /// Return the initial recursive watcher-ready evidence.
    pub const fn watcher_ready(self) -> LinuxWatcherReady {
        self.watcher_ready
    }

    /// Return the same-loop callback-drain evidence.
    pub const fn callback_drain(self) -> LinuxCallbackDrain {
        self.callback_drain
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Platform-discriminated native coverage evidence.
pub enum WatcherCoverageBoundary {
    /// FSEvents cursor overlap and replay evidence.
    Darwin(DarwinCoverageBoundary),
    /// Inotify watcher-ready and callback-drain evidence.
    Linux(LinuxCoverageBoundary),
}

impl WatcherCoverageBoundary {
    /// Return this boundary's globally monotonic lifecycle identity.
    pub const fn boundary_id(self) -> WatcherBoundaryId {
        match self {
            Self::Darwin(boundary) => boundary.boundary_id(),
            Self::Linux(boundary) => boundary.boundary_id(),
        }
    }

    /// Return the native stream that is live after this boundary.
    pub const fn live_stream_epoch(self) -> WatcherStreamEpoch {
        match self {
            Self::Darwin(boundary) => boundary.live_epoch(),
            Self::Linux(boundary) => boundary.stream_epoch(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Exact native validity generation captured for one mechanical boundary.
pub struct WatcherCoverageCloseToken {
    boundary_id: WatcherBoundaryId,
    generation: NonZeroU64,
}

impl WatcherCoverageCloseToken {
    /// Return the boundary this close token covers.
    pub const fn boundary_id(self) -> WatcherBoundaryId {
        self.boundary_id
    }

    /// Return the opaque native validity generation.
    pub const fn generation(self) -> u64 {
        self.generation.get()
    }
}

#[derive(Clone, Debug)]
/// Nonblocking native authority sampled inside the coordinator close section.
pub struct WatcherCoverageCloseGuard {
    authority: WatcherCoverageAuthority,
    token: WatcherCoverageCloseToken,
}

impl WatcherCoverageCloseGuard {
    /// Return the immutable close token for diagnostics and receipts.
    pub const fn token(&self) -> WatcherCoverageCloseToken {
        self.token
    }

    /// Whether no native invalidation linearized after this boundary.
    ///
    /// The coordinator calls this while holding its close mutex. If this load
    /// wins, a later native loss opens or continues recovery after that close;
    /// if native invalidation wins, this returns false and the close retries.
    pub fn is_current(&self) -> bool {
        self.authority.generation.load(Ordering::Acquire) == self.token.generation.get()
    }
}

#[derive(Clone, Debug)]
/// Mechanical native boundary plus its close-time validity authority.
pub struct WatcherCoverageHandoff {
    boundary: WatcherCoverageBoundary,
    close_guard: WatcherCoverageCloseGuard,
}

impl WatcherCoverageHandoff {
    pub(super) fn new(
        boundary: WatcherCoverageBoundary,
        close_guard: WatcherCoverageCloseGuard,
    ) -> Self {
        debug_assert_eq!(boundary.boundary_id(), close_guard.token().boundary_id());
        Self {
            boundary,
            close_guard,
        }
    }

    /// Return the immutable platform proof.
    pub const fn boundary(&self) -> WatcherCoverageBoundary {
        self.boundary
    }

    /// Return the native authority that must be sampled during close.
    pub const fn close_guard(&self) -> &WatcherCoverageCloseGuard {
        &self.close_guard
    }

    /// Return this handoff's globally monotonic boundary identity.
    pub const fn boundary_id(&self) -> WatcherBoundaryId {
        self.boundary.boundary_id()
    }

    /// Return the stream live after the handoff.
    pub const fn live_stream_epoch(&self) -> WatcherStreamEpoch {
        self.boundary.live_stream_epoch()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Native observations that invalidate coverage.
pub enum WatcherCoverageLoss {
    /// FSEvents dropped records in user space.
    UserDropped,
    /// FSEvents dropped records in the kernel.
    KernelDropped,
    /// FSEvents journal identifiers wrapped.
    EventIdsWrapped,
    /// The watched root moved, disappeared, or was replaced.
    RootChanged,
    /// The native watcher worker stopped.
    StreamStopped,
    /// A delivered FSEvents cursor moved backwards.
    NonMonotonicCursor,
    /// The inotify queue overflowed.
    QueueOverflow,
    /// The backend could not establish or drain a watch graph.
    BackendFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Fail-closed result of starting or maintaining a coverage boundary.
pub enum WatcherCoverageError {
    /// The platform cannot provide the required native primitive.
    CoverageUnavailable,
    /// The attempt deadline elapsed without a mechanical acknowledgement.
    TimedOut,
    /// The caller cancelled the attempt.
    Cancelled,
    /// A native loss observation invalidated the attempt.
    Loss(WatcherCoverageLoss),
    /// The boundary is not the adapter's current provisional boundary.
    StaleBoundary,
    /// A checked stream or boundary identity could not advance.
    IdentifierExhausted,
    /// The adapter has already shut down.
    Shutdown,
}

impl fmt::Display for WatcherCoverageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::CoverageUnavailable => "native watcher coverage is unavailable",
            Self::TimedOut => "native watcher coverage boundary timed out",
            Self::Cancelled => "native watcher coverage boundary was cancelled",
            Self::Loss(WatcherCoverageLoss::UserDropped) => {
                "FSEvents reported dropped user-space history"
            }
            Self::Loss(WatcherCoverageLoss::KernelDropped) => {
                "FSEvents reported dropped kernel history"
            }
            Self::Loss(WatcherCoverageLoss::EventIdsWrapped) => {
                "FSEvents journal identifiers wrapped"
            }
            Self::Loss(WatcherCoverageLoss::RootChanged) => "the watched root changed",
            Self::Loss(WatcherCoverageLoss::StreamStopped) => "the native watcher stream stopped",
            Self::Loss(WatcherCoverageLoss::NonMonotonicCursor) => {
                "the native watcher cursor moved backwards"
            }
            Self::Loss(WatcherCoverageLoss::QueueOverflow) => {
                "the native watcher event queue overflowed"
            }
            Self::Loss(WatcherCoverageLoss::BackendFailure) => "the native watcher backend failed",
            Self::StaleBoundary => "the native watcher boundary is stale",
            Self::IdentifierExhausted => "native watcher identifiers are exhausted",
            Self::Shutdown => "the native watcher adapter is shut down",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for WatcherCoverageError {}

#[derive(Clone, Debug)]
/// Cloneable cancellation signal for a bounded coverage attempt.
pub struct CoverageCancellation {
    inner: Arc<CoverageCancellationInner>,
}

#[derive(Debug)]
struct CoverageCancellationInner {
    cancelled: AtomicBool,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl CoverageCancellation {
    /// Create an uncancelled signal.
    pub fn new() -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Self {
            inner: Arc::new(CoverageCancellationInner {
                cancelled: AtomicBool::new(false),
                wake_tx,
                wake_rx,
            }),
        }
    }

    /// Cancel every wait sharing this signal and wake it immediately.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        let _ = self.inner.wake_tx.try_send(());
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }
}

impl Default for CoverageCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
/// Deadline and cancellation authority for one fail-closed attempt.
pub struct CoverageWait {
    deadline: Instant,
    cancellation: CoverageCancellation,
}

impl CoverageWait {
    /// Create a wait that may fail at `deadline` but cannot succeed from time.
    pub fn new(deadline: Instant, cancellation: CoverageCancellation) -> Self {
        Self {
            deadline,
            cancellation,
        }
    }
}

pub(super) fn wait_for_control<T>(
    wait: &CoverageWait,
    wake: &Receiver<()>,
    mut inspect: impl FnMut() -> Option<Result<T, WatcherCoverageError>>,
) -> Result<T, WatcherCoverageError> {
    loop {
        if wait.cancellation.is_cancelled() {
            return Err(WatcherCoverageError::Cancelled);
        }
        if Instant::now() >= wait.deadline {
            return Err(WatcherCoverageError::TimedOut);
        }
        if let Some(result) = inspect() {
            if result.is_ok() && Instant::now() >= wait.deadline {
                return Err(WatcherCoverageError::TimedOut);
            }
            return result;
        }
        let remaining = wait
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(WatcherCoverageError::TimedOut)?;
        let timeout = crossbeam_channel::after(remaining);
        crossbeam_channel::select! {
            recv(wake) -> wake_result => {
                if wake_result.is_err() {
                    return Err(WatcherCoverageError::CoverageUnavailable);
                }
            }
            recv(wait.cancellation.inner.wake_rx) -> _ => {}
            recv(timeout) -> _ => return Err(WatcherCoverageError::TimedOut),
        }
    }
}

/// Fault-injectable native coverage contract consumed by recovery coordination.
pub trait WatcherCoverageAdapter {
    /// Prepare a live native stream before the authoritative scan.
    fn begin_recovery(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError>;

    /// Seal native callback delivery after the authoritative scan.
    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError>;

    /// Revalidate that the exact promoted boundary remains live and current.
    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError>;

    /// Stop native workers and join them before returning.
    fn shutdown(&mut self) -> Result<(), WatcherCoverageError>;
}

/// Start the platform adapter and wait for its initial watcher-ready evidence.
pub fn start_native_adapter(
    root: &Path,
    event_handler: NativeEventHandler,
    ids: WatcherCoverageIds,
    wait: &CoverageWait,
) -> Result<NativeWatcherCoverageAdapter, WatcherCoverageError> {
    NativeWatcherCoverageAdapter::start(root, event_handler, ids, wait)
}

#[cfg(test)]
fn unique_test_root(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    std::env::temp_dir().join(format!(
        "{label}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn test_linux_handoff(
    boundary_id: u64,
    epoch: u64,
    ready_id: u64,
) -> WatcherCoverageHandoff {
    let boundary_id = WatcherBoundaryId(
        NonZeroU64::new(boundary_id).expect("test boundary identity must be nonzero"),
    );
    let ready_id =
        WatcherBoundaryId(NonZeroU64::new(ready_id).expect("test ready identity must be nonzero"));
    assert_ne!(
        ready_id, boundary_id,
        "watcher-ready and callback-drain identities must be distinct"
    );
    let boundary = WatcherCoverageBoundary::Linux(LinuxCoverageBoundary {
        boundary_id,
        stream_epoch: WatcherStreamEpoch(
            NonZeroU64::new(epoch).expect("test stream epoch must be nonzero"),
        ),
        watcher_ready: LinuxWatcherReady {
            control_id: ready_id,
        },
        callback_drain: LinuxCallbackDrain {
            control_id: boundary_id,
        },
    });
    let authority = WatcherCoverageAuthority::new();
    let close_guard = authority
        .close_guard(boundary_id)
        .expect("test close authority is available");
    WatcherCoverageHandoff::new(boundary, close_guard)
}

#[cfg(test)]
pub(crate) fn invalidate_test_handoff(handoff: &WatcherCoverageHandoff) {
    handoff.close_guard.authority.invalidate();
}
