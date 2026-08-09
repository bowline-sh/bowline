//! Bounded, parallel blob-upload pipeline (R6).
//!
//! Every buffered blob used to cost three strictly serialized round trips —
//! upload intent (Convex), create-only PUT (R2), metadata commit (Convex) — on
//! the single engine thread. A `git checkout` touching 5,000 files therefore
//! cost ~15,000 sequential RTTs, which at a 40 ms round trip is ten minutes of
//! doing nothing but waiting.
//!
//! Nucleus's split applies directly: keep the control thread synchronous and
//! deterministic *because* the I/O is offloaded. `put_blob` queues the sealed
//! bytes and returns; the queue drains through a scoped worker pool at the next
//! point where a failure could matter. The engine's `RemoteObjects` seam is
//! untouched, so the counting fake still tests the engine.
//!
//! **Ordering contract.** Nothing may reference an object the hosted service has
//! not recorded. Queued uploads are unreferenced by construction — the only
//! thing that can reference a blob is the manifest, and the only thing that can
//! reference a manifest is the ref CAS. Every one of those publishing paths
//! drains first (see `ManifestTransport`), so at the instant a manifest's
//! metadata row exists, every blob row it names already exists. A crash with a
//! queue still in memory strands nothing: the bytes were never named.

use std::cell::{Cell, RefCell};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use bowline_core::ids::ContentId;
use bowline_local::sync::manifest_engine::{KeyEpoch, TransportError};

use super::object_uploader::UploadKind;

/// How much sealed data may sit queued before the engine thread must drain.
///
/// This is the pipeline's memory ceiling, not a batch-size preference: the queue
/// holds owned copies of sealed bytes, so an unbounded queue would let one push
/// of a large tree hold the whole tree in RAM. Buffered blobs are below the
/// engine's 8 MiB large-file threshold, so this admits at least four of the
/// largest and thousands of typical source files.
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;

/// Second ceiling, for the small-file case the byte budget never trips. 61% of
/// source files are under 10 KB, so a byte-only bound would queue ~3,000 of them
/// and make the first drain a long stall with nothing overlapping it.
const MAX_QUEUED_OBJECTS: usize = 256;

/// Maximum network waves needed to drain the small-object queue.
///
/// Each object requires an intent, a create-only PUT, and a metadata commit.
/// The physical dense-producer proof showed that 32 waves of those round trips
/// can consume the entire 30-second recovery contract even after native
/// coverage and engine admission are bounded. Eight waves leaves most of the
/// contract for scanning, manifest publication, peer observation, and apply.
const MAX_UPLOAD_WAVES: usize = 8;

/// Concurrent in-flight uploads during a drain.
///
/// This is derived from both queue bounds rather than the laptop's core count:
/// the work is round-trip-bound, and a full small-object queue must fit inside
/// [`MAX_UPLOAD_WAVES`]. Memory remains bounded by [`MAX_QUEUED_BYTES`].
const UPLOAD_CONCURRENCY: usize = MAX_QUEUED_OBJECTS.div_ceil(MAX_UPLOAD_WAVES);

/// One queued sealed blob. Owns its bytes: the engine's borrow ends when
/// `put_blob` returns, and the worker that uploads it runs later.
pub(super) struct QueuedUpload {
    pub(super) kind: UploadKind,
    pub(super) content_id: ContentId,
    pub(super) key: String,
    pub(super) sealed: Vec<u8>,
    pub(super) key_epoch: KeyEpoch,
}

impl QueuedUpload {
    fn queued_bytes(&self) -> usize {
        self.sealed.len()
    }
}

/// What the caller must do after queueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueAdmission {
    /// The queue has room; keep sealing.
    Queued,
    /// A bound was reached; drain before queueing anything else.
    DrainNow,
}

/// The engine-thread-owned queue. `RefCell`/`Cell` rather than a lock because
/// every mutation happens on the single engine thread; the worker pool only ever
/// sees an already-taken `Vec`.
pub(super) struct UploadQueue {
    queued: RefCell<Vec<QueuedUpload>>,
    queued_bytes: Cell<usize>,
}

impl UploadQueue {
    pub(super) fn new() -> Self {
        Self {
            queued: RefCell::new(Vec::new()),
            queued_bytes: Cell::new(0),
        }
    }

    pub(super) fn push(&self, upload: QueuedUpload) -> QueueAdmission {
        let bytes = self
            .queued_bytes
            .get()
            .saturating_add(upload.queued_bytes());
        self.queued_bytes.set(bytes);
        let queued_objects = match self.queued.try_borrow_mut() {
            Ok(mut queued) => {
                queued.push(upload);
                queued.len()
            }
            // Only reachable if a drain re-entered `push`, which the call graph
            // forbids. Draining immediately is the safe interpretation.
            Err(_borrowed) => return QueueAdmission::DrainNow,
        };

        if bytes >= MAX_QUEUED_BYTES || queued_objects >= MAX_QUEUED_OBJECTS {
            QueueAdmission::DrainNow
        } else {
            QueueAdmission::Queued
        }
    }

    /// Remove everything queued so far, leaving the queue empty.
    pub(super) fn take(&self) -> Vec<QueuedUpload> {
        self.queued_bytes.set(0);
        self.queued
            .try_borrow_mut()
            .map(|mut queued| std::mem::take(&mut *queued))
            .unwrap_or_default()
    }
}

/// Run `upload_one` over every job across a bounded scoped worker pool.
///
/// Returns the failure belonging to the lowest job index, so the error a caller
/// sees does not depend on which worker lost the race — an engine that reports a
/// different path on every retry is an engine nobody can debug.
pub(super) fn map_in_parallel<T, F>(
    jobs: &[QueuedUpload],
    map_one: F,
) -> Result<Vec<T>, TransportError>
where
    T: Send,
    F: Fn(usize, &QueuedUpload) -> Result<T, TransportError> + Sync,
{
    map_with_concurrency(jobs, UPLOAD_CONCURRENCY, map_one)
}

pub(super) fn map_slice_in_parallel<J, T, F>(
    jobs: &[J],
    concurrency: usize,
    map_one: F,
) -> Result<Vec<T>, TransportError>
where
    J: Sync,
    T: Send,
    F: Fn(usize, &J) -> Result<T, TransportError> + Sync,
{
    map_with_concurrency(jobs, concurrency, map_one)
}

#[cfg(test)]
fn drain_with_concurrency<F>(
    jobs: &[QueuedUpload],
    concurrency: usize,
    upload_one: F,
) -> Result<(), TransportError>
where
    F: Fn(&QueuedUpload) -> Result<(), TransportError> + Sync,
{
    map_with_concurrency(jobs, concurrency, |_index, job| {
        upload_one(job)?;
        Ok(())
    })
    .map(|_| ())
}

fn map_with_concurrency<J, T, F>(
    jobs: &[J],
    concurrency: usize,
    map_one: F,
) -> Result<Vec<T>, TransportError>
where
    J: Sync,
    T: Send,
    F: Fn(usize, &J) -> Result<T, TransportError> + Sync,
{
    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let next_job = AtomicUsize::new(0);
    let failures: Mutex<Vec<(usize, TransportError)>> = Mutex::new(Vec::new());
    let results: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::with_capacity(jobs.len()));
    let workers = concurrency.clamp(1, jobs.len());

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next_job.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else { break };
                    match map_one(index, job) {
                        Ok(value) => record_result(&results, index, value),
                        Err(error) => {
                            record_failure(&failures, index, error);
                            // Stop pulling work: the whole push is already lost,
                            // and continuing only burns bandwidth on objects no
                            // ref will ever name.
                            break;
                        }
                    }
                }
            });
        }
    });

    lowest_indexed_failure(failures)?;
    let mut results = match results.into_inner() {
        Ok(results) => results,
        Err(poisoned) => poisoned.into_inner(),
    };
    results.sort_by_key(|(index, _value)| *index);
    Ok(results.into_iter().map(|(_index, value)| value).collect())
}

fn record_result<T>(results: &Mutex<Vec<(usize, T)>>, index: usize, value: T) {
    match results.lock() {
        Ok(mut results) => results.push((index, value)),
        Err(poisoned) => poisoned.into_inner().push((index, value)),
    }
}

fn record_failure(
    failures: &Mutex<Vec<(usize, TransportError)>>,
    index: usize,
    error: TransportError,
) {
    match failures.lock() {
        Ok(mut failures) => failures.push((index, error)),
        // A poisoned lock means a worker panicked mid-report. The push must
        // still fail, and it must fail with something a log can explain.
        Err(poisoned) => poisoned.into_inner().push((index, error)),
    }
}

fn lowest_indexed_failure(
    failures: Mutex<Vec<(usize, TransportError)>>,
) -> Result<(), TransportError> {
    let mut failures = match failures.into_inner() {
        Ok(failures) => failures,
        Err(poisoned) => poisoned.into_inner(),
    };
    failures.sort_by_key(|(index, _error)| *index);
    match failures.into_iter().next() {
        Some((_index, error)) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Condvar, Mutex as StdMutex};
    use std::time::Duration;

    use super::*;

    /// Bounded wait for observed parallelism. A `Barrier` would hang forever if
    /// the pipeline regressed to serial; this reports `false` instead, so the
    /// regression is a failed assertion rather than a hung CI job.
    struct ConcurrencyWitness {
        state: StdMutex<ConcurrencyState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct ConcurrencyState {
        active: usize,
        peak: usize,
    }

    impl ConcurrencyWitness {
        fn new() -> Self {
            Self {
                state: StdMutex::new(ConcurrencyState::default()),
                changed: Condvar::new(),
            }
        }

        /// Enter, wait until `expected` jobs are simultaneously inside, leave.
        fn observe(&self, expected: usize, timeout: Duration) {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.active += 1;
            state.peak = state.peak.max(state.active);
            self.changed.notify_all();
            while state.active < expected {
                let Ok((next, wait)) = self.changed.wait_timeout(state, timeout) else {
                    return;
                };
                state = next;
                if wait.timed_out() {
                    break;
                }
            }
            state.active -= 1;
            self.changed.notify_all();
        }

        fn peak(&self) -> usize {
            self.state.lock().map_or(0, |state| state.peak)
        }
    }

    fn job(index: usize) -> QueuedUpload {
        QueuedUpload {
            kind: UploadKind::Blob,
            content_id: ContentId::new(format!("cid_{index}")),
            key: format!("b_{}", "0".repeat(64)),
            sealed: vec![0_u8; 16],
            key_epoch: KeyEpoch::new(1),
        }
    }

    /// The R6 assertion: uploads must actually overlap. Under the old
    /// one-at-a-time transport the peak can only ever be 1.
    #[test]
    fn drain_runs_uploads_concurrently() {
        let jobs: Vec<QueuedUpload> = (0..16).map(job).collect();
        let witness = ConcurrencyWitness::new();

        drain_with_concurrency(&jobs, 4, |_job| {
            witness.observe(4, Duration::from_secs(5));
            Ok(())
        })
        .expect("drain succeeds");

        assert!(
            witness.peak() >= 4,
            "peak concurrency was {}, expected at least 4",
            witness.peak()
        );
    }

    #[test]
    fn full_small_object_queue_fits_the_recovery_upload_wave_budget() {
        const { assert!(UPLOAD_CONCURRENCY > 1) };
        assert!(MAX_QUEUED_OBJECTS.div_ceil(UPLOAD_CONCURRENCY) <= MAX_UPLOAD_WAVES);
    }

    #[test]
    fn drain_visits_every_job_exactly_once() {
        let jobs: Vec<QueuedUpload> = (0..64).map(job).collect();
        let visited = AtomicUsize::new(0);

        drain_with_concurrency(&jobs, 8, |_job| {
            visited.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .expect("drain succeeds");

        assert_eq!(visited.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn drain_reports_the_lowest_indexed_failure() {
        let jobs: Vec<QueuedUpload> = (0..8).map(job).collect();

        let error = drain_with_concurrency(&jobs, 8, |job| {
            let index: usize = job
                .content_id
                .as_str()
                .trim_start_matches("cid_")
                .parse()
                .expect("test content ids are indexed");
            if index >= 3 {
                Err(TransportError::new("put-blob", format!("failed {index}")))
            } else {
                Ok(())
            }
        })
        .expect_err("drain fails");

        assert!(error.to_string().contains("failed 3"), "{error}");
    }

    #[test]
    fn drain_of_an_empty_queue_does_no_work() {
        let visited = AtomicUsize::new(0);

        drain_with_concurrency(&[], 8, |_job| {
            visited.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .expect("empty drain succeeds");

        assert_eq!(visited.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_demands_a_drain_once_the_object_bound_is_reached() {
        let queue = UploadQueue::new();
        for index in 0..MAX_QUEUED_OBJECTS - 1 {
            assert_eq!(queue.push(job(index)), QueueAdmission::Queued);
        }

        assert_eq!(
            queue.push(job(MAX_QUEUED_OBJECTS)),
            QueueAdmission::DrainNow
        );
        assert_eq!(queue.take().len(), MAX_QUEUED_OBJECTS);
        assert!(queue.take().is_empty(), "take must leave the queue empty");
    }

    #[test]
    fn queue_demands_a_drain_once_the_byte_bound_is_reached() {
        let queue = UploadQueue::new();
        let mut oversized = job(0);
        oversized.sealed = vec![0_u8; MAX_QUEUED_BYTES];

        assert_eq!(queue.push(oversized), QueueAdmission::DrainNow);
    }
}
