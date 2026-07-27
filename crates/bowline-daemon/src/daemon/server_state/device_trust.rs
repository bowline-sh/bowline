//! The daemon's periodic answer to "what changed about who is trusted here":
//! refreshing the device trust list, and converging this device onto the
//! workspace key epoch a revocation named.

use bowline_control_plane::DeviceControlPlaneClient;
use std::time::Instant;

use bowline_control_plane::DeviceApprovalRequestList;

use bowline_daemon::status_projection::{StatusSourceFacts, StatusSourceState};

use crate::daemon::DaemonServerState;
use crate::daemon::hosted_control_plane;
use crate::daemon::key_store;
use crate::daemon::server_state::DEVICE_TRUST_REFRESH_INTERVAL;
use crate::daemon::server_state::device_trust_status_facts;
impl DaemonServerState {
    pub(in crate::daemon) fn refresh_device_trust_if_due(&self) {
        if self.cancels_side_work() {
            return;
        }
        let now = Instant::now();
        let due = self
            .next_device_trust_refresh
            .lock()
            .map(|mut next_refresh| {
                if now < *next_refresh {
                    return false;
                }
                *next_refresh = now + DEVICE_TRUST_REFRESH_INTERVAL;
                true
            })
            .unwrap_or(false);
        if !due {
            return;
        }
        if self.sync_options.is_none() {
            return;
        }
        let result = self.fetch_device_trust();
        if self.cancels_side_work() {
            return;
        }
        match result {
            Ok(trust) => {
                let facts = device_trust_status_facts(
                    &trust,
                    self.sync_options.as_ref().map(|args| args.root.as_path()),
                );
                self.update_projection_source(
                    &self.projection_sources.device_trust,
                    StatusSourceFacts::DeviceTrustDetails(facts),
                );
            }
            Err(()) => self.mark_device_trust_degraded(),
        }
        self.converge_workspace_key_epoch();
    }

    /// Rides the device-trust refresh because it answers the same question at
    /// the same cadence: what has changed about who is trusted here. A
    /// revocation names a new key epoch, and this is the pass that makes every
    /// remaining device reach it — with nothing asked of the user.
    fn converge_workspace_key_epoch(&self) {
        let Some((workspace_id, device_id)) = self.sync_identity() else {
            return;
        };
        let Ok(key_store) = key_store() else {
            return;
        };
        let Ok(control_plane) =
            hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())
        else {
            return;
        };
        // A failed pass is not an error state: the obligation is durable on the
        // control plane, so the next tick retries it. Reporting keeps a
        // persistent failure visible in the daemon log rather than silent.
        if let Err(error) = bowline_local::trust::converge_workspace_key_epoch(
            &control_plane,
            &*key_store,
            &workspace_id,
            &device_id,
        ) {
            eprintln!("bowline-daemon workspace key convergence deferred: {error}");
        }
    }

    fn fetch_device_trust(&self) -> Result<DeviceApprovalRequestList, ()> {
        let (workspace_id, device_id) = self.sync_identity().ok_or(())?;
        let key_store = key_store().map_err(|_| ())?;
        let control_plane =
            hosted_control_plane(&*key_store, workspace_id.clone(), device_id).map_err(|_| ())?;
        control_plane
            .list_device_trust(&workspace_id)
            .map_err(|_| ())
    }

    fn mark_device_trust_degraded(&self) {
        let Some(current) = self.projection_sources.device_trust.current() else {
            return;
        };
        let degraded = match current {
            StatusSourceFacts::DeviceTrust(mut facts) => {
                facts.state = StatusSourceState::Degraded;
                StatusSourceFacts::DeviceTrust(facts)
            }
            StatusSourceFacts::DeviceTrustDetails(mut facts) => {
                facts.state.state = StatusSourceState::Degraded;
                StatusSourceFacts::DeviceTrustDetails(facts)
            }
            _ => return,
        };
        self.update_projection_source(&self.projection_sources.device_trust, degraded);
    }
}
