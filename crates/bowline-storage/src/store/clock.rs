use std::{
    cell::RefCell,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
// LocalByteStore::open uses System mode in production. Deterministic mode also
// ships because cross-crate tests and the CLI dev spike construct deterministic
// stores outside this crate's cfg(test), matching the workspace's deterministic
// substrate precedent in bowline-control-plane.
pub(super) struct StoreClock {
    mode: ClockMode,
}

#[derive(Debug)]
enum ClockMode {
    System,
    Deterministic(RefCell<u64>),
}

impl StoreClock {
    pub(super) fn system() -> Self {
        Self {
            mode: ClockMode::System,
        }
    }

    pub(super) fn deterministic(start_unix_ms: u64) -> Self {
        Self {
            mode: ClockMode::Deterministic(RefCell::new(start_unix_ms)),
        }
    }

    pub(super) fn now_unix_ms(&self) -> u64 {
        match &self.mode {
            ClockMode::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after Unix epoch")
                .as_millis() as u64,
            ClockMode::Deterministic(next) => {
                // Return then increment so each deterministic read has a
                // strictly ordered created_at_unix_ms value.
                let mut next = next.borrow_mut();
                let current = *next;
                *next += 1;
                current
            }
        }
    }
}
