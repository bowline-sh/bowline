use bowline_control_plane::DeviceControlPlaneClient;
use bowline_core::wire::generated::{DaemonDeviceActionParams, DaemonRpcErrorCode};
use bowline_local::trust::grants;

use super::{
    CancellationPoint, DaemonServerState, RequestContext, RpcResult, checkpoint,
    request_context_error, rpc_error,
};
use crate::daemon::{current_timestamp, hosted_control_plane, key_store};

pub(super) fn device_action(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
    peer_credential_checked: bool,
    approve: bool,
) -> RpcResult<serde_json::Value> {
    if !peer_credential_checked {
        return Err(rpc_error(
            DaemonRpcErrorCode::PermissionDenied,
            "device actions require a verified same-user local socket peer",
            false,
        ));
    }
    let params = parse_params(params)?;
    let Some((workspace_id, device_id)) = state.sync_identity() else {
        return Err(rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "device actions require a configured daemon workspace",
            false,
        ));
    };
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let key_store = key_store().map_err(|error| {
        rpc_error(
            DaemonRpcErrorCode::Unavailable,
            &format!("device key store is unavailable: {error}"),
            true,
        )
    })?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Unavailable,
                &format!("device trust service is unavailable: {error}"),
                true,
            )
        })?;
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let trust = control_plane
        .list_device_trust(&workspace_id)
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Unavailable,
                &format!("device trust state is unavailable: {error}"),
                true,
            )
        })?;
    if !trust
        .pending_requests
        .iter()
        .any(|request| request.request_id.as_str() == params.request_id)
    {
        return Ok(serde_json::json!({
            "requestId": params.request_id,
            "state": "already-resolved",
        }));
    }

    if approve {
        context
            .begin_commit_fence()
            .map_err(|error| request_context_error(context, error))?;
        bowline_local::trust::approve_device_request(
            &control_plane,
            &*key_store,
            bowline_local::trust::ApproveDeviceOptions {
                workspace_id,
                request_id: bowline_core::ids::DeviceApprovalRequestId::new(
                    params.request_id.clone(),
                ),
                approver_device_id: device_id,
                generated_at: current_timestamp(),
            },
        )
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                &format!("device approval failed: {error}"),
                false,
            )
        })?;
    } else {
        checkpoint(context, CancellationPoint::BeforeExternalCall)?;
        let identity = key_store
            .load_or_create_device_identity()
            .map_err(|error| {
                rpc_error(
                    DaemonRpcErrorCode::Unavailable,
                    &format!("device identity is unavailable: {error}"),
                    true,
                )
            })?;
        let request_id = bowline_core::ids::DeviceApprovalRequestId::new(params.request_id.clone());
        let proof = grants::device_authorization_proof(
            &identity,
            &workspace_id,
            &device_id,
            "deny-device-request",
            &grants::device_request_proof_subject(&request_id),
        )
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                &format!("device denial proof failed: {error}"),
                false,
            )
        })?;
        context
            .begin_commit_fence()
            .map_err(|error| request_context_error(context, error))?;
        control_plane
            .deny_device_request(bowline_control_plane::DeviceDenialInput {
                request_id,
                denied_by_device_id: device_id,
                denied_by_device_proof: proof,
                reason: "denied by Bowline menu bar".to_string(),
            })
            .map_err(|error| {
                rpc_error(
                    DaemonRpcErrorCode::Internal,
                    &format!("device denial failed: {error}"),
                    false,
                )
            })?;
    }
    Ok(serde_json::json!({
        "requestId": params.request_id,
        "state": "resolved",
    }))
}

fn parse_params(params: serde_json::Value) -> RpcResult<DaemonDeviceActionParams> {
    let params = serde_json::from_value::<DaemonDeviceActionParams>(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "device action params are invalid",
            false,
        )
    })?;
    if params.request_id.is_empty()
        || params.request_id.len() > 512
        || params.idempotency_key.is_empty()
        || params.idempotency_key.len() > 128
    {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "device action identifiers are outside their bounded contract",
            false,
        ));
    }
    Ok(params)
}
