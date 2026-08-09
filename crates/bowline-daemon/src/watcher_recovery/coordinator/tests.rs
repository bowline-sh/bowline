use std::{thread, time::Duration};

use super::*;
use crate::watcher_recovery::{RecoveryRevision, test_support};

#[test]
fn delayed_older_publication_cannot_regress_or_strand_subscription() {
    let coordinator = WatcherRecoveryCoordinator::nominal(
        test_support::source_identity(),
        test_support::timestamp(),
        BackoffPolicy::standard(),
    );
    let ownership = coordinator
        .replace_projector()
        .expect("projector ownership must allocate");
    let subscription = coordinator
        .subscribe_projector(ownership)
        .expect("projector subscription must open");
    let older = RecoveryRevision::new(1).expect("revision must be valid");
    let newer = RecoveryRevision::new(2).expect("revision must be valid");

    coordinator.signal.publish(newer);
    coordinator.signal.publish(older);

    assert_eq!(subscription.try_recv(), Ok(newer));
    assert_eq!(
        subscription.try_recv(),
        Err(RecoverySubscriptionError::WouldBlock)
    );
}

#[test]
fn callback_storm_coalesces_snapshot_materialization_without_hiding_loss() {
    let coordinator = WatcherRecoveryCoordinator::nominal(
        test_support::source_identity(),
        test_support::timestamp(),
        BackoffPolicy::standard(),
    );
    let ownership = coordinator
        .replace_projector()
        .expect("projector ownership must allocate");
    let subscription = coordinator
        .subscribe_projector(ownership)
        .expect("projector subscription must open");
    coordinator
        .observe_loss(
            RecoveryCause::NativeCallbackLaneSaturated,
            test_support::moment(1),
        )
        .expect("first loss must open recovery");
    let recovering = coordinator
        .snapshot()
        .expect("loss snapshot must be immediately visible");
    assert_eq!(
        recovering.lifecycle(),
        super::super::RecoveryLifecycle::Recovering
    );
    assert_eq!(
        coordinator
            .state
            .lock()
            .expect("coordinator state must remain available")
            .snapshot_materializations,
        2
    );

    const ACTIVITY_EVENTS: u64 = 8_192;
    for offset in 0..ACTIVITY_EVENTS {
        coordinator
            .observe_activity(test_support::moment(offset + 2))
            .expect("storm activity must remain allocation-light");
    }
    assert_eq!(
        coordinator
            .state
            .lock()
            .expect("coordinator state must remain available")
            .snapshot_materializations,
        2,
        "callback ingress must not materialize one snapshot per event"
    );

    let published = subscription
        .try_recv()
        .expect("one level-triggered revision must cover the whole storm");
    let current = coordinator
        .snapshot()
        .expect("one reader must materialize the latest state");
    assert_eq!(published, current.snapshot_revision());
    assert_eq!(current.activity_watermark().get(), ACTIVITY_EVENTS + 1);
    assert_eq!(
        coordinator
            .state
            .lock()
            .expect("coordinator state must remain available")
            .snapshot_materializations,
        3
    );
}

#[test]
fn level_triggered_wake_reaches_worker_and_projector_roles() {
    let coordinator = Arc::new(WatcherRecoveryCoordinator::startup_reconciliation(
        test_support::source_identity(),
        test_support::moment(0),
        BackoffPolicy::standard(),
    ));
    let worker = coordinator
        .replace_worker(test_support::moment(1))
        .expect("worker ownership must allocate");
    let projector = coordinator
        .replace_projector()
        .expect("projector ownership must allocate");
    let subscriptions = vec![
        coordinator
            .subscribe_worker(worker)
            .expect("worker subscription must open"),
        coordinator
            .subscribe_projector(projector)
            .expect("projector subscription must open"),
    ];
    for subscription in &subscriptions {
        assert_eq!(
            subscription
                .try_recv()
                .expect("subscription must expose its current revision"),
            subscription.initial().snapshot_revision()
        );
    }
    let waiters = subscriptions
        .into_iter()
        .map(|subscription| {
            thread::spawn(move || subscription.recv_timeout(Duration::from_secs(1)))
        })
        .collect::<Vec<_>>();

    coordinator
        .observe_activity(test_support::moment(2))
        .expect("activity must publish one revision");
    let expected = coordinator
        .snapshot()
        .expect("snapshot must remain available")
        .snapshot_revision();
    for waiter in waiters {
        assert_eq!(
            waiter
                .join()
                .expect("subscriber thread must finish")
                .expect("subscriber must receive the revision"),
            expected
        );
    }
}

#[test]
fn roles_are_single_claim_and_reusable_without_anonymous_capacity() {
    let coordinator = WatcherRecoveryCoordinator::startup_reconciliation(
        test_support::source_identity(),
        test_support::moment(0),
        BackoffPolicy::standard(),
    );
    let worker = coordinator
        .replace_worker(test_support::moment(1))
        .expect("worker ownership must allocate");
    let projector = coordinator
        .replace_projector()
        .expect("projector ownership must allocate");
    let worker_subscription = coordinator
        .subscribe_worker(worker)
        .expect("worker subscription must open");
    let projector_subscription = coordinator
        .subscribe_projector(projector)
        .expect("projector subscription must open");
    assert!(matches!(
        coordinator.subscribe_worker(worker),
        Err(WatcherRecoveryCoordinatorError::RoleAlreadySubscribed {
            role: RecoverySubscriptionRole::Worker
        })
    ));
    assert!(matches!(
        coordinator.subscribe_projector(projector),
        Err(WatcherRecoveryCoordinatorError::RoleAlreadySubscribed {
            role: RecoverySubscriptionRole::Projector
        })
    ));

    drop(worker_subscription);
    let replacement = coordinator
        .subscribe_worker(worker)
        .expect("dropped worker claim must be reusable");
    assert_eq!(
        replacement
            .try_recv()
            .expect("current revision is immediate"),
        coordinator
            .snapshot()
            .expect("snapshot must be available")
            .snapshot_revision()
    );
    drop(projector_subscription);
}

#[test]
fn worker_replacement_revokes_live_receiver_and_starts_at_current_revision() {
    let coordinator = WatcherRecoveryCoordinator::startup_reconciliation(
        test_support::source_identity(),
        test_support::moment(0),
        BackoffPolicy::standard(),
    );
    let original = coordinator
        .replace_worker(test_support::moment(1))
        .expect("original worker must allocate");
    let stale = coordinator
        .subscribe_worker(original)
        .expect("original worker subscription must open");
    stale
        .try_recv()
        .expect("original worker sees current revision");
    let stale_waiter = thread::spawn(move || stale.recv_timeout(Duration::from_secs(1)));
    let projector = coordinator
        .replace_projector()
        .expect("projector ownership must allocate");
    let projector_subscription = coordinator
        .subscribe_projector(projector)
        .expect("projector subscription must remain occupied");
    let replacement = coordinator
        .replace_worker(test_support::moment(2))
        .expect("replacement worker must allocate");

    assert_eq!(
        stale_waiter.join().expect("stale waiter must finish"),
        Err(RecoverySubscriptionError::Revoked)
    );
    assert!(matches!(
        coordinator.subscribe_worker(original),
        Err(WatcherRecoveryCoordinatorError::SubscriptionRevoked {
            role: RecoverySubscriptionRole::Worker
        })
    ));
    let current = coordinator
        .subscribe_worker(replacement)
        .expect("replacement subscription must open");
    assert_eq!(
        current
            .try_recv()
            .expect("replacement sees current revision"),
        coordinator
            .snapshot()
            .expect("snapshot must remain available")
            .snapshot_revision()
    );
    assert!(projector_subscription.try_recv().is_ok());
}

#[test]
fn projector_replacement_revokes_live_receiver() {
    let coordinator = WatcherRecoveryCoordinator::nominal(
        test_support::source_identity(),
        test_support::timestamp(),
        BackoffPolicy::standard(),
    );
    let original = coordinator
        .replace_projector()
        .expect("original projector must allocate");
    let stale = coordinator
        .subscribe_projector(original)
        .expect("original projector subscription must open");
    let replacement = coordinator
        .replace_projector()
        .expect("replacement projector must allocate");

    assert_eq!(stale.try_recv(), Err(RecoverySubscriptionError::Revoked));
    coordinator
        .subscribe_projector(replacement)
        .expect("replacement projector subscription must open");
}
