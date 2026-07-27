use std::{
    collections::HashMap,
    io::{self, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

use bowline_core::wire::{
    generated::{DaemonRpcEvent, DaemonStatusEventPayload},
    status_command_to_wire,
};
use bowline_daemon_rpc::FrameCodec;

use super::{InFlightRequest, StatusSubscription, codec_io_error};

pub(super) fn next_wakeup(
    in_flight: &HashMap<String, InFlightRequest>,
    subscriptions: &HashMap<String, Arc<StatusSubscription>>,
    last_heartbeat: Instant,
    heartbeat_interval: Duration,
) -> Option<Duration> {
    let now = Instant::now();
    let request_deadline = in_flight
        .values()
        .filter(|request| !request.context.commit_fence_started())
        .filter_map(|request| request.context.deadline())
        .min();
    let heartbeat_deadline =
        (!subscriptions.is_empty()).then_some(last_heartbeat + heartbeat_interval);
    request_deadline
        .into_iter()
        .chain(heartbeat_deadline)
        .min()
        .map(|deadline| deadline.saturating_duration_since(now))
}

pub(super) fn flush_status_events(
    subscriptions: &mut HashMap<String, Arc<StatusSubscription>>,
    next_event_sequence: &mut u64,
    last_heartbeat: &mut Instant,
    heartbeat_interval: Duration,
    codec: FrameCodec,
    stream: &mut UnixStream,
) -> io::Result<()> {
    for subscription in subscriptions.values() {
        if let Some((snapshot, gap)) = subscription.take_pending() {
            write_event(
                codec,
                stream,
                subscription.id.clone(),
                *next_event_sequence,
                "status.snapshot",
                DaemonStatusEventPayload {
                    snapshot: Some(
                        status_command_to_wire(&snapshot.status)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                    ),
                    gap,
                    resync_required: gap,
                    heartbeat: false,
                },
            )?;
            *next_event_sequence = next_event_sequence.saturating_add(1);
            *last_heartbeat = Instant::now();
        }
    }
    subscriptions.retain(|_, subscription| !subscription.is_cancelled());

    if !subscriptions.is_empty() && last_heartbeat.elapsed() >= heartbeat_interval {
        for subscription in subscriptions.values() {
            write_event(
                codec,
                stream,
                subscription.id.clone(),
                *next_event_sequence,
                "status.heartbeat",
                DaemonStatusEventPayload {
                    snapshot: None,
                    gap: false,
                    resync_required: false,
                    heartbeat: true,
                },
            )?;
            *next_event_sequence = next_event_sequence.saturating_add(1);
        }
        *last_heartbeat = Instant::now();
    }
    Ok(())
}

fn write_event(
    codec: FrameCodec,
    stream: &mut UnixStream,
    subscription_id: String,
    sequence: u64,
    event_kind: &str,
    payload: DaemonStatusEventPayload,
) -> io::Result<()> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    codec
        .write(
            stream,
            &DaemonRpcEvent {
                subscription_id,
                sequence,
                event_kind: event_kind.to_string(),
                payload,
            },
        )
        .map_err(codec_io_error)?;
    stream.flush()
}
