use super::*;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bowline_control_plane::ControlPlaneTimestamp;

const WORKSPACE: &str = "workspace_code";
const LOCAL_DEVICE: &str = "device_local";
const SECOND_DEVICE: &str = "device_second";

/// A control plane whose authorized-device list can change after the client has
/// been built, which is exactly what trusting a second device does.
struct FakeDeviceTrust {
    authorized: Mutex<Vec<DeviceProofVerifier>>,
    reads: AtomicUsize,
    failure: Mutex<Option<ControlPlaneError>>,
}

impl FakeDeviceTrust {
    fn with(devices: &[&str]) -> Self {
        Self {
            authorized: Mutex::new(devices.iter().map(|device| verifier(device)).collect()),
            reads: AtomicUsize::new(0),
            failure: Mutex::new(None),
        }
    }

    fn authorize(&self, device_id: &str) {
        self.authorized
            .lock()
            .expect("fake trust list is uncontended in tests")
            .push(verifier(device_id));
    }

    fn fail_with(&self, error: ControlPlaneError) {
        *self
            .failure
            .lock()
            .expect("fake trust failure is uncontended in tests") = Some(error);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl AuthorizedDeviceSource for FakeDeviceTrust {
    fn authorized_devices(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<AuthorizedDeviceRecord>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self
            .failure
            .lock()
            .expect("fake trust failure is uncontended in tests")
            .clone()
        {
            return Err(error);
        }
        Ok(self
            .authorized
            .lock()
            .expect("fake trust list is uncontended in tests")
            .iter()
            .map(|verifier| AuthorizedDeviceRecord {
                workspace_id: workspace_id.clone(),
                device_id: verifier.device_id.clone(),
                device_name: verifier.device_id.as_str().to_string(),
                platform: "linux".to_string(),
                device_fingerprint: format!("fingerprint_{}", verifier.device_id.as_str()),
                authorized_at: ControlPlaneTimestamp { tick: 1 },
                authorized_by_device_id: None,
                device_authorization_proof_verifier: Some(verifier.proof_verifier.clone()),
                revoked_at: None,
            })
            .collect())
    }
}

fn verifier(device_id: &str) -> DeviceProofVerifier {
    DeviceProofVerifier {
        workspace_id: WorkspaceId::new(WORKSPACE),
        device_id: DeviceId::new(device_id),
        proof_verifier: format!("dapv_p256_v1_{device_id}"),
    }
}

fn discarding_store() -> VerifierStore {
    Arc::new(|_workspace_id, _verifiers| Ok(()))
}

/// A trust handle seeded exactly as a daemon client is built: this device's own
/// verifier and nothing else.
fn built_trust(store: VerifierStore) -> Arc<WorkspaceDeviceTrust> {
    let seed = verifier(LOCAL_DEVICE);
    let mut cache = DeviceProofVerifierCache::new();
    cache.insert(
        (Some(seed.workspace_id.clone()), seed.device_id.clone()),
        seed.proof_verifier.clone(),
    );
    WorkspaceDeviceTrust::new(WorkspaceId::new(WORKSPACE), cache, store)
}

fn resolved(trust: &Arc<WorkspaceDeviceTrust>, device_id: &str) -> Option<String> {
    trust.resolver()(&WorkspaceId::new(WORKSPACE), &DeviceId::new(device_id))
        .expect("verifier resolution does not fail")
}

#[test]
fn a_device_trusted_after_the_client_was_built_verifies_without_a_restart() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let trust = built_trust(discarding_store());
    trust
        .refresh(&control_plane)
        .expect("the build-time refresh reads the trust that exists now");
    assert_eq!(
        resolved(&trust, SECOND_DEVICE),
        None,
        "the second device does not exist yet"
    );

    // `bowline connect` on the second device, while this daemon keeps running.
    control_plane.authorize(SECOND_DEVICE);

    let outcome = trust.refresh_unknown_signer(
        &control_plane,
        &DeviceId::new(SECOND_DEVICE),
        Instant::now(),
    );
    assert!(
        outcome.learned(),
        "a device the control plane authorizes must be learned, not refused: {outcome}"
    );
    assert_eq!(
        resolved(&trust, SECOND_DEVICE),
        Some(verifier(SECOND_DEVICE).proof_verifier),
        "the running client's resolver must answer for the newly trusted device"
    );
    assert_eq!(
        resolved(&trust, LOCAL_DEVICE),
        Some(verifier(LOCAL_DEVICE).proof_verifier),
        "learning a peer must not drop this device's own verifier"
    );
}

#[test]
fn a_refreshed_verifier_set_is_stored_for_the_next_process() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE, SECOND_DEVICE]);
    let stored: Arc<Mutex<Vec<DeviceProofVerifier>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&stored);
    let trust = built_trust(Arc::new(move |_workspace_id, verifiers| {
        *sink.lock().expect("test store is uncontended") = verifiers.to_vec();
        Ok(())
    }));

    trust.refresh(&control_plane).expect("refresh");

    let stored = stored.lock().expect("test store is uncontended");
    assert_eq!(
        stored
            .iter()
            .map(|v| v.device_id.clone())
            .collect::<Vec<_>>(),
        vec![DeviceId::new(LOCAL_DEVICE), DeviceId::new(SECOND_DEVICE)]
    );
}

#[test]
fn an_unauthorized_signer_costs_exactly_one_control_plane_read() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let trust = built_trust(discarding_store());
    let stranger = DeviceId::new("device_stranger");
    let start = Instant::now();

    let first = trust.refresh_unknown_signer(&control_plane, &stranger, start);
    assert!(matches!(first, TrustRefreshOutcome::NotAuthorized));
    assert_eq!(control_plane.reads(), 1);

    // The stream keeps pushing the same unverifiable head, twice a minute.
    // Everything inside the refusal window is answered from memory.
    for attempt in 1..1_000_u32 {
        let elapsed = Duration::from_millis(u64::from(attempt) * 500);
        assert!(elapsed < REFUSED_SIGNER_RETRY_INTERVAL);
        let outcome = trust.refresh_unknown_signer(&control_plane, &stranger, start + elapsed);
        assert!(
            matches!(outcome, TrustRefreshOutcome::AlreadyRefused),
            "attempt {attempt} must be refused from memory: {outcome}"
        );
    }
    assert_eq!(
        control_plane.reads(),
        1,
        "a signer the control plane disowned costs one read per refusal window"
    );
}

/// The refusal is bounded in time, not permanent: a device approved after it
/// was refused has to become verifiable without anyone restarting the daemon.
/// The cost of that is capped at one read per window.
#[test]
fn a_permanently_unverifiable_signer_costs_one_read_per_refusal_window() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let trust = built_trust(discarding_store());
    let stranger = DeviceId::new("device_stranger");
    let start = Instant::now();
    let a_day = Duration::from_secs(24 * 60 * 60);
    let pushes = 2_880_u32;

    for attempt in 0..pushes {
        let outcome = trust.refresh_unknown_signer(
            &control_plane,
            &stranger,
            start + Duration::from_secs(u64::from(attempt) * 30),
        );
        assert!(!outcome.learned(), "attempt {attempt}: {outcome}");
    }

    let windows = u32::try_from(a_day.as_secs() / REFUSED_SIGNER_RETRY_INTERVAL.as_secs())
        .expect("a day is a small number of windows");
    let reads = u32::try_from(control_plane.reads()).expect("read count fits");
    assert!(
        reads <= windows + 1,
        "a day of unverifiable heads cost {reads} reads, more than one per window"
    );
    assert!(
        reads * 10 < pushes,
        "the reads must be a small fraction of the {pushes} pushes that triggered them"
    );
}

/// A refusal must stay remembered when the daemon also holds trust for ANOTHER
/// workspace, which is the ordinary case for anyone with two workspaces.
///
/// Refusals are cleared whenever a read shows this workspace's trust changed,
/// because a change can also change the answer for a device refused earlier.
/// That comparison is only meaningful if both sides describe the same
/// workspace: a snapshot that leaked other workspaces' verifiers could never
/// equal the single-workspace set it was compared against, so every refresh
/// looked like a trust change and wiped refusals that nothing had invalidated.
/// A signer already refused then gets asked about again — the per-signer
/// backoff silently degraded to the workspace cooldown, and only ever visible
/// once a second workspace is in the cache.
#[test]
fn a_refusal_is_remembered_when_another_workspace_is_cached() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let seed = verifier(LOCAL_DEVICE);
    let mut cache = DeviceProofVerifierCache::new();
    cache.insert(
        (Some(seed.workspace_id.clone()), seed.device_id.clone()),
        seed.proof_verifier.clone(),
    );
    // A second workspace this daemon also holds trust for. Nothing here
    // refreshes it, so it must not enter this workspace's accounting at all.
    cache.insert(
        (
            Some(WorkspaceId::new("workspace_other")),
            DeviceId::new("device_elsewhere"),
        ),
        "dapv_p256_v1_device_elsewhere".to_string(),
    );
    let trust = WorkspaceDeviceTrust::new(WorkspaceId::new(WORKSPACE), cache, discarding_store());

    let start = Instant::now();
    let first = DeviceId::new("device_stranger_first");
    let second = DeviceId::new("device_stranger_second");

    let refused = trust.refresh_unknown_signer(&control_plane, &first, start);
    assert!(
        matches!(refused, TrustRefreshOutcome::NotAuthorized),
        "{refused}"
    );

    // A different unknown signer one cooldown later. Its own read is expected;
    // what it must NOT do is discard what was already learned about `first`,
    // because this workspace's trust did not change between the two reads.
    let other = trust.refresh_unknown_signer(
        &control_plane,
        &second,
        start + UNKNOWN_SIGNER_REFRESH_COOLDOWN,
    );
    assert!(
        matches!(other, TrustRefreshOutcome::NotAuthorized),
        "{other}"
    );

    let again = trust.refresh_unknown_signer(
        &control_plane,
        &first,
        start + UNKNOWN_SIGNER_REFRESH_COOLDOWN * 2,
    );
    assert!(
        matches!(again, TrustRefreshOutcome::AlreadyRefused),
        "a refusal nothing invalidated must survive another workspace being cached: {again}"
    );
    assert_eq!(
        control_plane.reads(),
        2,
        "re-asking about an already-refused signer costs a control-plane read"
    );
}

#[test]
fn distinct_unknown_signers_are_paced_by_the_workspace_cooldown() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let trust = built_trust(discarding_store());
    let start = Instant::now();

    // Every value names a device id nobody has seen before, so the per-signer
    // memory cannot bound this on its own.
    for attempt in 0..200_u32 {
        let device_id = DeviceId::new(format!("device_forged_{attempt}"));
        let outcome = trust.refresh_unknown_signer(
            &control_plane,
            &device_id,
            start + Duration::from_millis(u64::from(attempt) * 100),
        );
        assert!(
            matches!(
                outcome,
                TrustRefreshOutcome::NotAuthorized | TrustRefreshOutcome::RateLimited
            ),
            "forged signer {attempt}: {outcome}"
        );
    }

    // 200 values over 20s: one read for the first, and nothing else inside the
    // cooldown window.
    assert_eq!(control_plane.reads(), 1);
}

#[test]
fn a_signer_refused_before_a_trust_change_is_asked_about_once_more() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    let trust = built_trust(discarding_store());
    let start = Instant::now();
    let second = DeviceId::new(SECOND_DEVICE);

    let refused = trust.refresh_unknown_signer(&control_plane, &second, start);
    assert!(matches!(refused, TrustRefreshOutcome::NotAuthorized));

    // Another device is trusted later, which changes the workspace's trust and
    // makes every earlier refusal stale evidence.
    control_plane.authorize("device_third");
    let third = trust.refresh_unknown_signer(
        &control_plane,
        &DeviceId::new("device_third"),
        start + UNKNOWN_SIGNER_REFRESH_COOLDOWN,
    );
    assert!(third.learned(), "{third}");

    control_plane.authorize(SECOND_DEVICE);
    let relearned = trust.refresh_unknown_signer(
        &control_plane,
        &second,
        start + UNKNOWN_SIGNER_REFRESH_COOLDOWN * 2,
    );
    assert!(
        relearned.learned(),
        "a refusal must not outlive the trust it was based on: {relearned}"
    );
}

#[test]
fn a_failed_trust_read_leaves_the_signer_unknown_and_retryable() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE]);
    control_plane.fail_with(ControlPlaneError::Timeout {
        capability: "list-device-trust",
    });
    let trust = built_trust(discarding_store());
    let start = Instant::now();
    let second = DeviceId::new(SECOND_DEVICE);

    let outcome = trust.refresh_unknown_signer(&control_plane, &second, start);
    assert!(
        matches!(outcome, TrustRefreshOutcome::Unavailable(_)),
        "{outcome}"
    );
    assert!(
        !outcome.refused(),
        "an unreachable control plane says nothing about the device"
    );

    let too_soon = trust.refresh_unknown_signer(&control_plane, &second, start);
    assert!(matches!(too_soon, TrustRefreshOutcome::RateLimited));
    assert_eq!(control_plane.reads(), 1);

    *control_plane
        .failure
        .lock()
        .expect("fake trust failure is uncontended in tests") = None;
    control_plane.authorize(SECOND_DEVICE);
    let recovered = trust.refresh_unknown_signer(
        &control_plane,
        &second,
        start + UNKNOWN_SIGNER_REFRESH_COOLDOWN,
    );
    assert!(recovered.learned(), "{recovered}");
}

#[test]
fn a_revoked_device_stops_resolving_after_a_refresh() {
    let control_plane = FakeDeviceTrust::with(&[LOCAL_DEVICE, SECOND_DEVICE]);
    let trust = built_trust(discarding_store());
    trust.refresh(&control_plane).expect("refresh");
    assert!(resolved(&trust, SECOND_DEVICE).is_some());

    control_plane
        .authorized
        .lock()
        .expect("fake trust list is uncontended in tests")
        .retain(|verifier| verifier.device_id.as_str() != SECOND_DEVICE);
    trust.refresh(&control_plane).expect("refresh");

    assert_eq!(
        resolved(&trust, SECOND_DEVICE),
        None,
        "a refresh is authoritative in both directions"
    );
}
