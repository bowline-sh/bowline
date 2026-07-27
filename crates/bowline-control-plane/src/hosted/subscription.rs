//! The long-lived workspace-ref subscription.
//!
//! A one-shot hosted call proves its account session on every request, so
//! [`HostedControlPlaneClient::call_reauthenticating`] can replace a refused
//! session between two attempts of the same call. A subscription proves it once,
//! when the websocket opens, and then holds that credential for as long as the
//! connection lives. Recovery therefore has a different shape here: tear the
//! websocket down, replace the session, open a new one.

use super::*;

pub struct WorkspaceRefStreamShutdown(Option<tokio::sync::oneshot::Sender<()>>);

pub struct WorkspaceRefStreamCancellation(pub(super) tokio::sync::oneshot::Receiver<()>);

/// Connection lifecycle emitted by the Convex websocket that owns a workspace
/// ref subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRefStreamConnectionState {
    Connecting,
    Connected,
}

/// Ordered output from one workspace-ref subscription. Connection lifecycle and
/// values share one channel so consumers cannot attribute a value to the wrong
/// websocket generation.
#[derive(Debug)]
pub enum WorkspaceRefStreamEvent {
    ConnectionState(WorkspaceRefStreamConnectionState),
    Ref(ControlPlaneResult<Option<WorkspaceRef>>),
}

pub(super) enum WorkspaceRefStreamOutput {
    Values(std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>),
    Events(std::sync::mpsc::Sender<WorkspaceRefStreamEvent>),
}

impl WorkspaceRefStreamOutput {
    fn send_state(&self, state: WorkspaceRefStreamConnectionState) -> bool {
        match self {
            Self::Values(_) => true,
            Self::Events(sender) => sender
                .send(WorkspaceRefStreamEvent::ConnectionState(state))
                .is_ok(),
        }
    }

    fn send_ref(&self, value: ControlPlaneResult<Option<WorkspaceRef>>) -> bool {
        match self {
            Self::Values(sender) => sender.send(value).is_ok(),
            Self::Events(sender) => sender.send(WorkspaceRefStreamEvent::Ref(value)).is_ok(),
        }
    }
}

pub fn workspace_ref_stream_shutdown_pair()
-> (WorkspaceRefStreamShutdown, WorkspaceRefStreamCancellation) {
    let (shutdown, cancellation) = tokio::sync::oneshot::channel();
    (
        WorkspaceRefStreamShutdown(Some(shutdown)),
        WorkspaceRefStreamCancellation(cancellation),
    )
}

impl Drop for WorkspaceRefStreamShutdown {
    fn drop(&mut self) {
        if let Some(shutdown) = self.0.take() {
            let _already_stopped = shutdown.send(());
        }
    }
}

/// Why one websocket generation of the subscription ended.
pub(super) enum SubscriptionOutcome {
    /// Shutdown was requested, the websocket ended, or the consumer hung up.
    /// Nothing about the credential is in question.
    Ended,
    /// The control plane refused the account session this generation opened
    /// with. Only a replacement credential and a new websocket can clear it.
    AuthRejected(ControlPlaneError),
}

/// Everything one websocket generation of the subscription needs. The encoded
/// request belongs to exactly one generation because it carries the account
/// session that generation authenticates with.
struct WorkspaceRefAttempt<'a> {
    deployment_url: &'a str,
    device_proof_verifier_resolver: Option<&'a DeviceProofVerifierResolver>,
    output: &'a WorkspaceRefStreamOutput,
    request_args: ConvexArgs,
}

type StreamShutdown = std::pin::Pin<Box<tokio::sync::oneshot::Receiver<()>>>;

impl HostedControlPlaneClient {
    pub fn stream_workspace_ref_updates(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>,
    ) -> ControlPlaneResult<()> {
        let (_keepalive, shutdown) = workspace_ref_stream_shutdown_pair();
        self.stream_workspace_ref_updates_until(workspace_id, sender, shutdown)
    }

    pub fn stream_workspace_ref_updates_until(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        self.stream_workspace_ref_output_until(
            workspace_id,
            WorkspaceRefStreamOutput::Values(sender),
            shutdown,
        )
    }

    pub fn stream_workspace_ref_events_until(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<WorkspaceRefStreamEvent>,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        self.stream_workspace_ref_output_until(
            workspace_id,
            WorkspaceRefStreamOutput::Events(sender),
            shutdown,
        )
    }

    fn stream_workspace_ref_output_until(
        &self,
        workspace_id: &str,
        output: WorkspaceRefStreamOutput,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        let mut shutdown = Box::pin(shutdown.0);
        // The live subscription shares the typed refs:getWorkspaceRef contract:
        // the request is encoded through the endpoint marker and each pushed
        // value is decoded and head-signature verified by the same DTO boundary
        // as the one-shot query path.
        self.subscribe_reauthenticating(workspace_id, |request_args| {
            self.runtime.block_on(run_workspace_ref_attempt(
                WorkspaceRefAttempt {
                    deployment_url: &self.deployment_url,
                    device_proof_verifier_resolver: self.device_proof_verifier_resolver.as_ref(),
                    output: &output,
                    request_args,
                },
                &mut shutdown,
            ))
        })
    }

    /// Opens the subscription, and reopens it once under a replacement account
    /// session if the control plane refuses the one it opened with.
    ///
    /// Exactly one replacement per call, for the same reason
    /// [`HostedControlPlaneClient::call_reauthenticating`] retries once: a
    /// control plane that refuses a freshly registered session is refusing the
    /// identity, not the session, and reopening again would only spin against
    /// it. The refusal is returned instead, so the caller's reconnect backoff
    /// owns the wait and its status surface carries the condition.
    pub(super) fn subscribe_reauthenticating<F>(
        &self,
        workspace_id: &str,
        mut attempt: F,
    ) -> ControlPlaneResult<()>
    where
        F: FnMut(ConvexArgs) -> ControlPlaneResult<SubscriptionOutcome>,
    {
        let mut replaced_session = false;
        loop {
            let request = generated::HostedRefsGetWorkspaceRefRequest {
                workspace_id: workspace_id.to_string(),
                account_session_id: self.verified_account_session_id(Some(workspace_id))?,
            };
            let request_args = encode_hosted_request::<generated::RefsGetWorkspaceRef>(&request)?;
            let SubscriptionOutcome::AuthRejected(refusal) = attempt(request_args)? else {
                return Ok(());
            };
            if replaced_session || !self.reregister_account_session(Some(workspace_id))? {
                return Err(refusal);
            }
            replaced_session = true;
        }
    }
}

async fn run_workspace_ref_attempt(
    attempt: WorkspaceRefAttempt<'_>,
    shutdown: &mut StreamShutdown,
) -> ControlPlaneResult<SubscriptionOutcome> {
    let function_name = generated::RefsGetWorkspaceRef::CONVEX_FUNCTION;
    let (websocket_state_tx, mut websocket_state_rx) = tokio::sync::mpsc::channel(8);
    let Some(client) = until_workspace_ref_stream_shutdown(
        shutdown,
        ConvexClientBuilder::new(attempt.deployment_url)
            .with_on_state_change(websocket_state_tx)
            .build(),
    )
    .await
    else {
        return Ok(SubscriptionOutcome::Ended);
    };
    let mut client = client.map_err(map_convex_error)?;
    let Some(subscription) = until_workspace_ref_stream_shutdown(
        shutdown,
        client.subscribe(function_name, attempt.request_args),
    )
    .await
    else {
        return Ok(SubscriptionOutcome::Ended);
    };
    let mut subscription = subscription.map_err(map_convex_error)?;
    let mut websocket_state_open = true;
    loop {
        tokio::select! {
            biased;
            _ = &mut *shutdown => return Ok(SubscriptionOutcome::Ended),
            state = websocket_state_rx.recv(), if websocket_state_open => {
                let Some(state) = state else {
                    websocket_state_open = false;
                    continue;
                };
                let state = match state {
                    WebSocketState::Connected => WorkspaceRefStreamConnectionState::Connected,
                    WebSocketState::Connecting => WorkspaceRefStreamConnectionState::Connecting,
                };
                if !attempt.output.send_state(state) {
                    return Ok(SubscriptionOutcome::Ended);
                }
            }
            result = subscription.next() => {
                let Some(result) = result else {
                    return Ok(SubscriptionOutcome::Ended);
                };
                let pushed = parse_workspace_ref_push(
                    function_name,
                    result,
                    attempt.device_proof_verifier_resolver,
                );
                match pushed {
                    // Retryability is the single classification point: an
                    // ordinary transport drop must keep the caller's backoff and
                    // must not burn a session registration.
                    Err(refusal) if refusal.retryability() == Retryability::AuthExpired => {
                        return Ok(SubscriptionOutcome::AuthRejected(refusal));
                    }
                    pushed => {
                        if !attempt.output.send_ref(pushed) {
                            return Ok(SubscriptionOutcome::Ended);
                        }
                    }
                }
            }
        }
    }
}

fn parse_workspace_ref_push(
    function_name: &'static str,
    result: FunctionResult,
    device_proof_verifier_resolver: Option<&DeviceProofVerifierResolver>,
) -> ControlPlaneResult<Option<WorkspaceRef>> {
    let value = unwrap_function_result(function_name, result)?;
    let Some(dto) = decode_hosted_response::<generated::RefsGetWorkspaceRef>(value)? else {
        return Ok(None);
    };
    workspace_ref_from_dto(dto, |workspace_id, device_id| {
        let Some(resolver) = device_proof_verifier_resolver else {
            return Ok(None);
        };
        resolver(workspace_id, device_id)
    })
    .map(Some)
}

pub(super) async fn until_workspace_ref_stream_shutdown<T>(
    shutdown: &mut StreamShutdown,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match futures::future::select(shutdown.as_mut(), Box::pin(future)).await {
        Either::Left((_shutdown, _pending)) => None,
        Either::Right((output, _shutdown)) => Some(output),
    }
}
