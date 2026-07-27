use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvError, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonRpcCancel, DaemonRpcError, DaemonRpcEvent, DaemonRpcRequest,
    DaemonRpcResponse, DaemonServerHello, MACHINE_CONTRACT_VERSION, WIRE_SCHEMA_HASH,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    CodecError, DAEMON_RPC_PROTOCOL, DAEMON_RPC_PROTOCOL_VERSION, FrameCodec,
    negotiation::{VersionDimension, VersionWindow},
};

type PendingResult = Result<DaemonRpcResponse, ClientError>;
const MAX_ORPHAN_EVENT_SUBSCRIPTIONS: usize = 64;

pub struct ClientOptions {
    pub client_kind: String,
    pub client_version: String,
    pub capabilities: Vec<String>,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub max_frame_bytes: usize,
}

impl ClientOptions {
    pub fn new(client_kind: impl Into<String>, client_version: impl Into<String>) -> Self {
        Self {
            client_kind: client_kind.into(),
            client_version: client_version.into(),
            capabilities: Vec::new(),
            handshake_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: crate::DEFAULT_MAX_FRAME_BYTES,
        }
    }

    fn hello(&self) -> DaemonClientHello {
        DaemonClientHello {
            protocol: DAEMON_RPC_PROTOCOL.to_string(),
            protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
            contract_version: MACHINE_CONTRACT_VERSION,
            schema_hash: WIRE_SCHEMA_HASH.to_string(),
            client_kind: self.client_kind.clone(),
            client_version: self.client_version.clone(),
            capabilities: self.capabilities.clone(),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Codec(CodecError),
    SerializeParams(serde_json::Error),
    DeserializeResult(serde_json::Error),
    InvalidHandshake(String),
    /// The daemon's selected generation falls outside this build's window.
    IncompatibleVersion {
        dimension: VersionDimension,
        peer_version: u16,
        window: VersionWindow,
    },
    InvalidResponse {
        request_id: String,
        reason: &'static str,
    },
    Remote(Box<DaemonRpcError>),
    Timeout {
        request_id: String,
        timeout: Duration,
    },
    ConnectionClosed {
        message: String,
    },
    InternalState(&'static str),
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => {
                write!(formatter, "daemon RPC {operation} failed: {source}")
            }
            Self::Codec(source) => write!(formatter, "daemon RPC codec failed: {source}"),
            Self::SerializeParams(source) => write!(
                formatter,
                "daemon RPC params serialization failed: {source}"
            ),
            Self::DeserializeResult(source) => {
                write!(formatter, "daemon RPC result decoding failed: {source}")
            }
            Self::InvalidHandshake(reason) => {
                write!(formatter, "daemon RPC handshake is invalid: {reason}")
            }
            Self::IncompatibleVersion {
                dimension,
                peer_version,
                window,
            } => write!(
                formatter,
                "daemon {dimension} version {peer_version} is outside the supported range {window}"
            ),
            Self::InvalidResponse { request_id, reason } => write!(
                formatter,
                "daemon RPC response `{request_id}` is invalid: {reason}"
            ),
            Self::Remote(error) => write!(
                formatter,
                "daemon RPC returned {}: {}",
                error.code, error.message
            ),
            Self::Timeout {
                request_id,
                timeout,
            } => write!(
                formatter,
                "daemon RPC request `{request_id}` exceeded {timeout:?}"
            ),
            Self::ConnectionClosed { message } => {
                write!(formatter, "daemon RPC connection closed: {message}")
            }
            Self::InternalState(reason) => {
                write!(formatter, "daemon RPC internal state failed: {reason}")
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Codec(source) => Some(source),
            Self::SerializeParams(source) | Self::DeserializeResult(source) => Some(source),
            _ => None,
        }
    }
}

impl From<CodecError> for ClientError {
    fn from(source: CodecError) -> Self {
        Self::Codec(source)
    }
}

struct Shared {
    writer: Mutex<UnixStream>,
    pending: Mutex<HashMap<String, mpsc::Sender<PendingResult>>>,
    event_senders: Mutex<HashMap<String, Arc<EventMailbox>>>,
    orphan_events: Mutex<HashMap<String, DaemonRpcEvent>>,
    codec: FrameCodec,
    next_request_id: AtomicU64,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let stream = match self.writer.get_mut() {
            Ok(stream) => stream,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Clone)]
pub struct DaemonClient {
    shared: Arc<Shared>,
    hello: DaemonServerHello,
    request_timeout: Duration,
}

/// Whether `metadata` describes a Unix domain socket. A symlink is never
/// followed to answer this: the check exists to refuse a path that cannot host a
/// daemon, and following one would let a link decide the answer.
#[cfg(unix)]
fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    metadata.file_type().is_socket()
}

#[cfg(not(unix))]
fn is_socket(_metadata: &std::fs::Metadata) -> bool {
    true
}

impl DaemonClient {
    pub fn connect(path: &Path, options: ClientOptions) -> Result<Self, ClientError> {
        // A path that exists but is not a socket can never host a daemon, and the
        // kernel does not say so consistently: Linux answers ECONNREFUSED for a
        // directory, which is indistinguishable from "socket present, nobody
        // listening" and would be reported as `stopped`. A caller told `stopped`
        // starts a daemon; it needs to be told the path itself is wrong.
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && !is_socket(&metadata)
        {
            return Err(ClientError::Io {
                operation: "connect",
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon socket path is not a socket",
                ),
            });
        }
        let mut stream = UnixStream::connect(path).map_err(|source| ClientError::Io {
            operation: "connect",
            source,
        })?;
        stream
            .set_read_timeout(Some(options.handshake_timeout))
            .map_err(|source| ClientError::Io {
                operation: "set handshake read timeout",
                source,
            })?;
        stream
            .set_write_timeout(Some(options.handshake_timeout))
            .map_err(|source| ClientError::Io {
                operation: "set handshake write timeout",
                source,
            })?;

        let codec = FrameCodec::new(options.max_frame_bytes);
        codec.write_magic(&mut stream)?;
        codec.write(&mut stream, &options.hello())?;
        let handshake_value: serde_json::Value = codec.read(&mut stream)?;
        let hello = decode_server_hello(handshake_value)?;
        validate_server_selection(&hello)?;

        stream
            .set_read_timeout(None)
            .map_err(|source| ClientError::Io {
                operation: "clear read timeout",
                source,
            })?;
        stream
            .set_write_timeout(Some(options.request_timeout))
            .map_err(|source| ClientError::Io {
                operation: "set request write timeout",
                source,
            })?;
        let writer = stream.try_clone().map_err(|source| ClientError::Io {
            operation: "clone socket",
            source,
        })?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            event_senders: Mutex::new(HashMap::new()),
            orphan_events: Mutex::new(HashMap::new()),
            codec,
            next_request_id: AtomicU64::new(1),
        });
        let weak = Arc::downgrade(&shared);
        thread::Builder::new()
            .name("bowline-daemon-rpc-reader".to_string())
            .spawn(move || reader_loop(stream, weak, codec))
            .map_err(|source| ClientError::Io {
                operation: "spawn response reader",
                source,
            })?;
        Ok(Self {
            shared,
            hello,
            request_timeout: options.request_timeout,
        })
    }

    pub fn server_hello(&self) -> &DaemonServerHello {
        &self.hello
    }

    pub fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        timeout: Option<Duration>,
    ) -> Result<R, ClientError> {
        let timeout = timeout.unwrap_or(self.request_timeout);
        let request_id = format!(
            "request-{}",
            self.shared.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let request = DaemonRpcRequest {
            request_id: request_id.clone(),
            method: method.to_string(),
            params: serde_json::to_value(params).map_err(ClientError::SerializeParams)?,
            deadline_ms: deadline_millis(timeout),
        };
        let (sender, receiver) = mpsc::channel();
        self.pending()?.insert(request_id.clone(), sender);
        if let Err(error) = self.write(&request) {
            self.remove_pending(&request_id);
            return Err(error);
        }
        let response = match receiver.recv_timeout(timeout) {
            Ok(response) => response?,
            Err(RecvTimeoutError::Timeout) => {
                self.remove_pending(&request_id);
                let _ = self.cancel(&request_id);
                return Err(ClientError::Timeout {
                    request_id,
                    timeout,
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ClientError::ConnectionClosed {
                    message: "response router stopped".to_string(),
                });
            }
        };
        decode_response(response)
    }

    pub fn cancel(&self, request_id: &str) -> Result<(), ClientError> {
        self.write(&DaemonRpcCancel {
            request_id: request_id.to_string(),
        })
    }

    /// Register a latest-only mailbox for a subscription's events.
    ///
    /// Status events are level-triggered snapshots, so the mailbox deliberately
    /// holds exactly one event and marks the replacement as a gap. There is no
    /// queue and no capacity to choose.
    pub fn register_latest_event(
        &self,
        subscription_id: impl Into<String>,
    ) -> Result<EventReceiver, ClientError> {
        let subscription_id = subscription_id.into();
        let mailbox = Arc::new(EventMailbox::default());
        self.event_senders()?
            .insert(subscription_id.clone(), Arc::clone(&mailbox));
        if let Some(event) = self
            .shared
            .orphan_events
            .lock()
            .map_err(|_| ClientError::InternalState("orphan event lock is poisoned"))?
            .remove(&subscription_id)
        {
            mailbox.publish(event);
        }
        Ok(EventReceiver {
            subscription_id,
            mailbox,
            shared: Arc::downgrade(&self.shared),
        })
    }

    fn write<T: Serialize>(&self, value: &T) -> Result<(), ClientError> {
        let mut writer = self
            .shared
            .writer
            .lock()
            .map_err(|_| ClientError::InternalState("socket writer lock is poisoned"))?;
        self.shared.codec.write(&mut *writer, value)?;
        Ok(())
    }

    fn pending(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, mpsc::Sender<PendingResult>>>, ClientError>
    {
        self.shared
            .pending
            .lock()
            .map_err(|_| ClientError::InternalState("pending request lock is poisoned"))
    }

    fn event_senders(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Arc<EventMailbox>>>, ClientError> {
        self.shared
            .event_senders
            .lock()
            .map_err(|_| ClientError::InternalState("event router lock is poisoned"))
    }

    fn remove_pending(&self, request_id: &str) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.remove(request_id);
        }
    }
}

pub struct EventReceiver {
    subscription_id: String,
    mailbox: Arc<EventMailbox>,
    shared: Weak<Shared>,
}

impl EventReceiver {
    pub fn recv(&self) -> Result<DaemonRpcEvent, RecvError> {
        let mut state = self.mailbox.state.lock().map_err(|_| RecvError)?;
        loop {
            if let Some(event) = state.event.take() {
                return Ok(event);
            }
            if state.disconnected {
                return Err(RecvError);
            }
            state = self.mailbox.changed.wait(state).map_err(|_| RecvError)?;
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<DaemonRpcEvent, RecvTimeoutError> {
        let state = self
            .mailbox
            .state
            .lock()
            .map_err(|_| RecvTimeoutError::Disconnected)?;
        let (mut state, wait) = self
            .mailbox
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.event.is_none() && !state.disconnected
            })
            .map_err(|_| RecvTimeoutError::Disconnected)?;
        if let Some(event) = state.event.take() {
            return Ok(event);
        }
        if state.disconnected {
            Err(RecvTimeoutError::Disconnected)
        } else if wait.timed_out() {
            Err(RecvTimeoutError::Timeout)
        } else {
            Err(RecvTimeoutError::Disconnected)
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade()
            && let Ok(mut senders) = shared.event_senders.lock()
        {
            senders.remove(&self.subscription_id);
        }
        self.mailbox.disconnect();
    }
}

#[derive(Default)]
struct EventMailbox {
    state: Mutex<EventMailboxState>,
    changed: Condvar,
}

#[derive(Default)]
struct EventMailboxState {
    event: Option<DaemonRpcEvent>,
    disconnected: bool,
}

impl EventMailbox {
    fn publish(&self, mut event: DaemonRpcEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.event.is_some() {
            mark_event_gap(&mut event);
        }
        state.event = Some(event);
        self.changed.notify_one();
    }

    fn disconnect(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.disconnected = true;
            self.changed.notify_all();
        }
    }
}

fn mark_event_gap(event: &mut DaemonRpcEvent) {
    let Some(payload) = event.payload.as_object_mut() else {
        return;
    };
    if payload.contains_key("gap") {
        payload.insert("gap".to_string(), serde_json::Value::Bool(true));
    }
    if payload.contains_key("resyncRequired") {
        payload.insert("resyncRequired".to_string(), serde_json::Value::Bool(true));
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ServerFrame {
    Response(DaemonRpcResponse),
    Event(DaemonRpcEvent),
}

fn reader_loop(mut stream: UnixStream, shared: Weak<Shared>, codec: FrameCodec) {
    loop {
        let frame = match codec.read::<ServerFrame, _>(&mut stream) {
            Ok(frame) => frame,
            Err(error) => {
                fail_pending(&shared, error.to_string());
                return;
            }
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match frame {
            ServerFrame::Response(response) => {
                let sender = shared
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.remove(&response.request_id));
                if let Some(sender) = sender {
                    let _ = sender.send(Ok(response));
                }
            }
            ServerFrame::Event(mut event) => {
                let subscription_id = event.subscription_id.clone();
                let sender = shared
                    .event_senders
                    .lock()
                    .ok()
                    .and_then(|senders| senders.get(&subscription_id).cloned());
                if let Some(sender) = sender {
                    sender.publish(event);
                } else if let Ok(mut orphan_events) = shared.orphan_events.lock() {
                    let replaces_existing = orphan_events.contains_key(&subscription_id);
                    if replaces_existing {
                        mark_event_gap(&mut event);
                    }
                    if replaces_existing || orphan_events.len() < MAX_ORPHAN_EVENT_SUBSCRIPTIONS {
                        orphan_events.insert(subscription_id, event);
                    }
                }
            }
        }
    }
}

fn fail_pending(shared: &Weak<Shared>, message: String) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let pending = shared
        .pending
        .lock()
        .map(|mut pending| {
            pending
                .drain()
                .map(|(_, sender)| sender)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for sender in pending {
        let _ = sender.send(Err(ClientError::ConnectionClosed {
            message: message.clone(),
        }));
    }
    let mailboxes = shared
        .event_senders
        .lock()
        .map(|senders| senders.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for mailbox in mailboxes {
        mailbox.disconnect();
    }
}

fn decode_server_hello(value: serde_json::Value) -> Result<DaemonServerHello, ClientError> {
    if let Ok(hello) = serde_json::from_value::<DaemonServerHello>(value.clone()) {
        return Ok(hello);
    }
    if let Ok(error) = serde_json::from_value::<DaemonRpcError>(value) {
        return Err(ClientError::Remote(Box::new(error)));
    }
    Err(ClientError::InvalidHandshake(
        "frame is neither a server hello nor a structured error".to_string(),
    ))
}

/// Accept the daemon's selected generations if they sit inside this build's own
/// compatibility windows. The daemon selects the lower of the two peers'
/// supported versions; this is the client half of that agreement.
///
/// `schema_hash` is informational for the same reason it is on the server side:
/// it digests the whole contract corpus, including hosted documents the daemon
/// RPC surface never touches.
fn validate_server_selection(hello: &DaemonServerHello) -> Result<(), ClientError> {
    check_window(VersionDimension::Protocol, hello.protocol_version)?;
    check_window(VersionDimension::MachineContract, hello.contract_version)
}

fn check_window(dimension: VersionDimension, peer_version: u16) -> Result<(), ClientError> {
    let window = dimension.window();
    window
        .select(peer_version)
        .map(|_selected| ())
        .ok_or(ClientError::IncompatibleVersion {
            dimension,
            peer_version,
            window,
        })
}

fn decode_response<R: DeserializeOwned>(response: DaemonRpcResponse) -> Result<R, ClientError> {
    match (response.result, response.error) {
        (Some(result), None) => {
            serde_json::from_value(result).map_err(ClientError::DeserializeResult)
        }
        (None, Some(error)) => Err(ClientError::Remote(Box::new(error))),
        (Some(_), Some(_)) => Err(ClientError::InvalidResponse {
            request_id: response.request_id,
            reason: "both result and error are present",
        }),
        (None, None) => Err(ClientError::InvalidResponse {
            request_id: response.request_id,
            reason: "neither result nor error is present",
        }),
    }
}

fn deadline_millis(timeout: Duration) -> Option<u32> {
    let millis = timeout.as_millis().clamp(1, 120_000);
    Some(millis as u32)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
