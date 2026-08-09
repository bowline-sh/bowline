use loom::{
    model,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use super::{
    CloseDisposition, RecoveryCause, RecoveryLifecycle, RecoveryRevision, RecoveryScanRevision,
    test_support::{drive_to_closing, moment, startup_model},
};

struct LoomRevisionSignal {
    revision: AtomicU64,
}

impl LoomRevisionSignal {
    fn new() -> Self {
        Self {
            revision: AtomicU64::new(RecoveryRevision::INITIAL.get()),
        }
    }

    fn publish(&self, candidate: RecoveryRevision) {
        self.revision.fetch_max(candidate.get(), Ordering::Release);
    }

    fn wait_after(&self, last_seen: RecoveryRevision) -> RecoveryRevision {
        loop {
            let revision = RecoveryRevision::from_valid(self.revision.load(Ordering::Acquire));
            if revision > last_seen {
                return revision;
            }
            thread::yield_now();
        }
    }

    fn current(&self) -> RecoveryRevision {
        RecoveryRevision::from_valid(self.revision.load(Ordering::Acquire))
    }
}

#[test]
fn reordered_revision_publications_never_regress_or_strand_waiter() {
    model(|| {
        let signal = Arc::new(LoomRevisionSignal::new());
        let waiter_signal = Arc::clone(&signal);
        let waiter = thread::spawn(move || waiter_signal.wait_after(RecoveryRevision::INITIAL));
        let older_signal = Arc::clone(&signal);
        let older = thread::spawn(move || {
            older_signal.publish(RecoveryRevision::new(1).expect("revision must be valid"));
        });
        let newer_signal = Arc::clone(&signal);
        let newer = thread::spawn(move || {
            newer_signal.publish(RecoveryRevision::new(2).expect("revision must be valid"));
        });

        older.join().expect("older publisher must finish");
        newer.join().expect("newer publisher must finish");
        let observed = waiter.join().expect("waiter must finish");
        assert!(observed > RecoveryRevision::INITIAL);
        assert_eq!(signal.current().get(), 2);
    });
}

#[test]
fn loss_and_close_have_no_false_nominal_interleaving() {
    let mut initial = startup_model();
    let closing = drive_to_closing(&mut initial, 1);

    model(move || {
        let shared = Arc::new(Mutex::new(initial.clone()));
        let close_state = Arc::clone(&shared);
        let close_offer = closing.close_offer.clone();
        let close = thread::spawn(move || {
            close_state
                .lock()
                .expect("model mutex must remain available")
                .offer_close(&close_offer, moment(20))
        });
        let loss_state = Arc::clone(&shared);
        let loss = thread::spawn(move || {
            loss_state
                .lock()
                .expect("model mutex must remain available")
                .observe_loss(RecoveryCause::WatcherDisconnected, moment(20))
        });

        let close_result = close.join().expect("close thread must finish");
        loss.join()
            .expect("loss thread must finish")
            .expect("loss must be admitted");
        assert!(matches!(
            close_result,
            Ok(CloseDisposition::Closed(_) | CloseDisposition::RetryRequired { .. })
        ));
        let state = shared.lock().expect("model mutex must remain available");
        assert_eq!(state.lifecycle(), RecoveryLifecycle::Recovering);
        assert!(state.has_open_incident());
        assert!(state.invariant_holds());
    });
}

#[test]
fn stale_attempt_close_cannot_close_a_replacement_attempt() {
    let mut initial = startup_model();
    let stale = drive_to_closing(&mut initial, 1);
    initial
        .observe_suppressed(moment(7))
        .expect("a suppressed write must invalidate the first attempt");
    assert!(matches!(
        initial
            .offer_close(&stale.close_offer, moment(8))
            .expect("stale close must request retry"),
        CloseDisposition::RetryRequired { .. }
    ));
    initial
        .start_attempt(moment(9))
        .expect("replacement attempt must start");

    model(move || {
        let shared = Arc::new(Mutex::new(initial.clone()));
        let stale_state = Arc::clone(&shared);
        let stale_offer = stale.close_offer.clone();
        let stale_close = thread::spawn(move || {
            let _ = stale_state
                .lock()
                .expect("model mutex must remain available")
                .offer_close(&stale_offer, moment(10));
        });
        let loss_state = Arc::clone(&shared);
        let loss = thread::spawn(move || {
            loss_state
                .lock()
                .expect("model mutex must remain available")
                .observe_loss(RecoveryCause::RootReplaced, moment(10))
        });

        stale_close.join().expect("stale close thread must finish");
        loss.join()
            .expect("loss thread must finish")
            .expect("loss must be admitted");
        let state = shared.lock().expect("model mutex must remain available");
        assert_eq!(state.lifecycle(), RecoveryLifecycle::Recovering);
        assert!(state.current_attempt().is_some());
        assert!(state.invariant_holds());
    });
}

#[test]
fn loss_and_native_boundary_admission_have_one_total_order() {
    let mut initial = startup_model();
    let token = initial
        .start_attempt(moment(1))
        .expect("attempt must start");
    initial
        .record_scan_started(token, moment(1))
        .expect("scan must start");
    initial
        .record_scan_completed(
            token,
            RecoveryScanRevision::new(1).expect("revision must be valid"),
            moment(1),
        )
        .expect("scan must complete");
    let handoff = super::test_support::linux_handoff(1);

    model(move || {
        let handoff = handoff.clone();
        let shared = Arc::new(Mutex::new(initial.clone()));
        let activity_state = Arc::clone(&shared);
        let activity = thread::spawn(move || {
            activity_state
                .lock()
                .expect("model mutex must remain available")
                .observe_suppressed(moment(2))
        });
        let boundary_state = Arc::clone(&shared);
        let admit = thread::spawn(move || {
            boundary_state
                .lock()
                .expect("model mutex must remain available")
                .record_native_boundary(token, handoff, moment(2))
        });

        activity
            .join()
            .expect("activity thread must finish")
            .expect("activity must be admitted");
        admit
            .join()
            .expect("boundary thread must finish")
            .expect("boundary must be admitted");

        let state = shared.lock().expect("model mutex must remain available");
        let boundary = state
            .current_native_boundary()
            .expect("boundary must remain attached to the attempt");
        assert!(boundary.activity_watermark() <= state.activity_watermark());
        assert!(state.snapshot().rescan_required());
        assert!(state.invariant_holds());
    });
}
