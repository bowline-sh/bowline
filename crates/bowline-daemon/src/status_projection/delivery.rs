use std::{
    fmt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{RecvError, RecvTimeoutError, TryRecvError},
    },
    time::Duration,
};

use super::types::DaemonStatusProjection;

pub(crate) type Projection = Arc<DaemonStatusProjection>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    Coalesced,
    Disconnected,
}

#[derive(Debug)]
struct LatestProjectionSlot {
    pending: Mutex<Option<Projection>>,
    ready: Condvar,
    wake: Mutex<Option<ProjectionWake>>,
    receiver_alive: AtomicBool,
    sender_alive: AtomicBool,
}

#[derive(Clone)]
struct ProjectionWake(Arc<dyn Fn() + Send + Sync + 'static>);

impl fmt::Debug for ProjectionWake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProjectionWake(..)")
    }
}

#[derive(Debug)]
pub(crate) struct LatestProjectionSender {
    slot: Arc<LatestProjectionSlot>,
}

#[derive(Debug)]
pub struct LatestProjectionReceiver {
    slot: Arc<LatestProjectionSlot>,
}

pub(crate) fn latest_projection_channel() -> (LatestProjectionSender, LatestProjectionReceiver) {
    let slot = Arc::new(LatestProjectionSlot {
        pending: Mutex::new(None),
        ready: Condvar::new(),
        wake: Mutex::new(None),
        receiver_alive: AtomicBool::new(true),
        sender_alive: AtomicBool::new(true),
    });
    (
        LatestProjectionSender {
            slot: Arc::clone(&slot),
        },
        LatestProjectionReceiver { slot },
    )
}

impl LatestProjectionSender {
    pub(crate) fn is_connected(&self) -> bool {
        self.slot.receiver_alive.load(Ordering::Acquire)
    }

    pub(crate) fn deliver(&self, projection: Projection) -> DeliveryOutcome {
        if !self.slot.receiver_alive.load(Ordering::Acquire) {
            return DeliveryOutcome::Disconnected;
        }
        let Ok(mut pending) = self.slot.pending.lock() else {
            return DeliveryOutcome::Disconnected;
        };
        if !self.slot.receiver_alive.load(Ordering::Acquire) {
            return DeliveryOutcome::Disconnected;
        }
        let outcome = if pending.replace(projection).is_some() {
            DeliveryOutcome::Coalesced
        } else {
            DeliveryOutcome::Delivered
        };
        drop(pending);
        self.slot.ready.notify_one();
        if let Ok(wake) = self.slot.wake.lock()
            && let Some(wake) = wake.as_ref()
        {
            (wake.0)();
        }
        outcome
    }
}

impl LatestProjectionReceiver {
    pub fn set_wake(&self, wake: Option<Arc<dyn Fn() + Send + Sync + 'static>>) {
        if let Ok(mut registered) = self.slot.wake.lock() {
            *registered = wake.map(ProjectionWake);
        }
    }

    pub fn recv(&self) -> Result<Projection, RecvError> {
        let Ok(pending) = self.slot.pending.lock() else {
            return Err(RecvError);
        };
        let Ok(mut pending) = self.slot.ready.wait_while(pending, |value| {
            value.is_none() && self.slot.sender_alive.load(Ordering::Acquire)
        }) else {
            return Err(RecvError);
        };
        pending.take().ok_or(RecvError)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Projection, RecvTimeoutError> {
        let Ok(pending) = self.slot.pending.lock() else {
            return Err(RecvTimeoutError::Disconnected);
        };
        let Ok((mut pending, _wait)) =
            self.slot
                .ready
                .wait_timeout_while(pending, timeout, |value| {
                    value.is_none()
                        && self.slot.receiver_alive.load(Ordering::Acquire)
                        && self.slot.sender_alive.load(Ordering::Acquire)
                })
        else {
            return Err(RecvTimeoutError::Disconnected);
        };
        if let Some(projection) = pending.take() {
            return Ok(projection);
        }
        if self.slot.sender_alive.load(Ordering::Acquire) {
            Err(RecvTimeoutError::Timeout)
        } else {
            Err(RecvTimeoutError::Disconnected)
        }
    }

    pub fn try_recv(&self) -> Result<Projection, TryRecvError> {
        let Ok(mut pending) = self.slot.pending.lock() else {
            return Err(TryRecvError::Disconnected);
        };
        if let Some(projection) = pending.take() {
            return Ok(projection);
        }
        if self.slot.sender_alive.load(Ordering::Acquire) {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Disconnected)
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.slot
            .pending
            .lock()
            .map_or(0, |pending| usize::from(pending.is_some()))
    }
}

impl Drop for LatestProjectionSender {
    fn drop(&mut self) {
        self.slot.sender_alive.store(false, Ordering::Release);
        self.slot.ready.notify_all();
    }
}

impl Drop for LatestProjectionReceiver {
    fn drop(&mut self) {
        self.slot.receiver_alive.store(false, Ordering::Release);
        if let Ok(mut wake) = self.slot.wake.lock() {
            wake.take();
        }
        if let Ok(mut pending) = self.slot.pending.lock() {
            pending.take();
        }
        self.slot.ready.notify_all();
    }
}
