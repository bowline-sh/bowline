use std::{
    fmt,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ControlPlaneTimestamp {
    pub tick: u64,
}

impl fmt::Display for ControlPlaneTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "t{:012}", self.tick)
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicClock {
    next_tick: Arc<Mutex<u64>>,
}

impl DeterministicClock {
    pub fn new(start_tick: u64) -> Self {
        Self {
            next_tick: Arc::new(Mutex::new(start_tick)),
        }
    }

    pub fn now(&self) -> ControlPlaneTimestamp {
        let mut next_tick = self.next_tick.lock().expect("deterministic clock poisoned");
        let timestamp = ControlPlaneTimestamp { tick: *next_tick };
        *next_tick += 1;
        timestamp
    }

    pub fn peek(&self) -> ControlPlaneTimestamp {
        let next_tick = self.next_tick.lock().expect("deterministic clock poisoned");
        ControlPlaneTimestamp { tick: *next_tick }
    }
}

impl Default for DeterministicClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicIdGenerator {
    prefix: String,
    next_id: Arc<Mutex<u64>>,
}

impl DeterministicIdGenerator {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: sanitize_id_part(&prefix.into()),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    pub fn next_id(&self, kind: &str) -> String {
        let mut next_id = self
            .next_id
            .lock()
            .expect("deterministic ID generator poisoned");
        let id = format!("{}-{}-{:08}", self.prefix, sanitize_id_part(kind), *next_id);
        *next_id += 1;
        id
    }
}

impl Default for DeterministicIdGenerator {
    fn default() -> Self {
        Self::new("bowline")
    }
}

pub(crate) fn sanitize_id_part(value: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_dash = false;

    for character in value.chars() {
        let next = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else {
            '-'
        };

        if next == '-' {
            if !last_was_dash {
                sanitized.push(next);
            }
            last_was_dash = true;
        } else {
            sanitized.push(next);
            last_was_dash = false;
        }
    }

    sanitized = sanitized.trim_matches('-').to_string();

    if sanitized.is_empty() {
        "id".to_string()
    } else {
        sanitized
    }
}
