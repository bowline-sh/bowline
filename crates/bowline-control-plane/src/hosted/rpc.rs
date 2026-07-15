use super::wire_validation::validate_wire_value;
use super::*;
use serde_json::Value as JsonValue;
use std::{future::Future, pin::Pin};

pub(super) type ConvexArgs = BTreeMap<String, Value>;
pub(super) type CachedRpcCallFuture =
    Pin<Box<dyn Future<Output = ControlPlaneResult<FunctionResult>>>>;

pub(super) async fn rpc_with_cached_client<C, Connect, ConnectFuture, Call>(
    cached_client: &TokioMutex<Option<C>>,
    retry_after_transport_failure: bool,
    connect: Connect,
    call: Call,
) -> ControlPlaneResult<Value>
where
    C: Clone,
    Connect: Fn() -> ConnectFuture,
    ConnectFuture: Future<Output = ControlPlaneResult<C>>,
    Call: Fn(C) -> CachedRpcCallFuture,
{
    rpc_with_cached_client_after(
        cached_client,
        retry_after_transport_failure,
        CONVEX_RPC_TIMEOUT,
        connect,
        call,
    )
    .await
}

pub(super) async fn rpc_with_cached_client_after<C, Connect, ConnectFuture, Call>(
    cached_client: &TokioMutex<Option<C>>,
    retry_after_transport_failure: bool,
    timeout: Duration,
    connect: Connect,
    call: Call,
) -> ControlPlaneResult<Value>
where
    C: Clone,
    Connect: Fn() -> ConnectFuture,
    ConnectFuture: Future<Output = ControlPlaneResult<C>>,
    Call: Fn(C) -> CachedRpcCallFuture,
{
    let deadline = tokio::time::Instant::now() + timeout;
    match call_cached_rpc_once(cached_client, deadline, &connect, &call).await {
        Ok(result) => unwrap_function_result(result),
        Err(error) => {
            clear_cached_client(cached_client).await;
            if !retry_after_transport_failure || is_rpc_timeout_error(&error) {
                return Err(error);
            }
            match call_cached_rpc_once(cached_client, deadline, &connect, &call).await {
                Ok(result) => unwrap_function_result(result),
                Err(error) => {
                    clear_cached_client(cached_client).await;
                    Err(error)
                }
            }
        }
    }
}

async fn call_cached_rpc_once<C, Connect, ConnectFuture, Call>(
    cached_client: &TokioMutex<Option<C>>,
    deadline: tokio::time::Instant,
    connect: &Connect,
    call: &Call,
) -> ControlPlaneResult<FunctionResult>
where
    C: Clone,
    Connect: Fn() -> ConnectFuture,
    ConnectFuture: Future<Output = ControlPlaneResult<C>>,
    Call: Fn(C) -> CachedRpcCallFuture,
{
    let client = cloned_cached_client(cached_client, deadline, connect).await?;
    tokio::time::timeout(remaining_rpc_timeout(deadline)?, call(client))
        .await
        .map_err(|_| rpc_timeout_error())?
}

async fn cloned_cached_client<C, Connect, ConnectFuture>(
    cached_client: &TokioMutex<Option<C>>,
    deadline: tokio::time::Instant,
    connect: &Connect,
) -> ControlPlaneResult<C>
where
    C: Clone,
    Connect: Fn() -> ConnectFuture,
    ConnectFuture: Future<Output = ControlPlaneResult<C>>,
{
    {
        let cached = cached_client.lock().await;
        if let Some(client) = cached.as_ref() {
            return Ok(client.clone());
        }
    }
    let client = tokio::time::timeout(remaining_rpc_timeout(deadline)?, connect())
        .await
        .map_err(|_| rpc_timeout_error())??;
    let mut cached = cached_client.lock().await;
    if let Some(client) = cached.as_ref() {
        return Ok(client.clone());
    }
    *cached = Some(client.clone());
    Ok(client)
}

async fn clear_cached_client<C>(cached_client: &TokioMutex<Option<C>>) {
    *cached_client.lock().await = None;
}

fn rpc_timeout_error() -> ControlPlaneError {
    ControlPlaneError::Timeout {
        capability: HOSTED_CAPABILITY,
    }
}

fn remaining_rpc_timeout(deadline: tokio::time::Instant) -> ControlPlaneResult<Duration> {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .ok_or_else(rpc_timeout_error)
}

fn is_rpc_timeout_error(error: &ControlPlaneError) -> bool {
    matches!(error, ControlPlaneError::Timeout { capability } if *capability == HOSTED_CAPABILITY)
}

pub(super) async fn call_convex_rpc(
    client: &mut ConvexClient,
    kind: ConvexRpcKind,
    name: &str,
    args: ConvexArgs,
) -> ControlPlaneResult<FunctionResult> {
    match kind {
        ConvexRpcKind::Query => client.query(name, args).await,
        ConvexRpcKind::Mutation => client.mutation(name, args).await,
        ConvexRpcKind::Action => client.action(name, args).await,
    }
    .map_err(map_convex_error)
}

#[cfg(test)]
pub(super) fn args<const N: usize>(entries: [(&'static str, Value); N]) -> ConvexArgs {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

pub(super) fn encode_hosted_request<E: HostedEndpoint>(
    request: &E::Request,
) -> ControlPlaneResult<ConvexArgs> {
    let encoded = serde_json::to_value(request)
        .map_err(|_| hosted_contract_error::<E>(HostedContractFailure::RequestEncoding, None))?;
    // Enforce the declared contract on the outbound request before transport.
    if let Err(violation) = validate_wire_value(E::REQUEST_SCHEMA, &encoded) {
        return Err(hosted_contract_error::<E>(
            HostedContractFailure::RequestEncoding,
            Some(violation.path()),
        ));
    }
    let serde_json::Value::Object(fields) = encoded else {
        return Err(hosted_contract_error::<E>(
            HostedContractFailure::RequestEncoding,
            None,
        ));
    };
    fields
        .into_iter()
        .map(|(name, value)| {
            // Wire numbers map to Convex `v.number()` (Float64); emit integers as
            // floats so the argument validator accepts them, matching the
            // hand-written `number_value` encoder.
            Value::try_from(integers_to_floats(value))
                .map(|value| (name, value))
                .map_err(|_| {
                    hosted_contract_error::<E>(HostedContractFailure::RequestEncoding, None)
                })
        })
        .collect()
}

fn integers_to_floats(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Number(number) if !number.is_f64() => {
            match number.as_f64().and_then(serde_json::Number::from_f64) {
                Some(float) => JsonValue::Number(float),
                None => JsonValue::Number(number),
            }
        }
        JsonValue::Array(items) => {
            JsonValue::Array(items.into_iter().map(integers_to_floats).collect())
        }
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, integers_to_floats(value)))
                .collect(),
        ),
        other => other,
    }
}

pub(super) fn decode_hosted_response<E: HostedEndpoint>(
    response: Value,
) -> ControlPlaneResult<E::Response> {
    let json = normalize_integral_floats(response.into());
    // Enforce the declared contract on the decoded response before returning it;
    // Serde only checks structure, not the semantic bounds/format constraints.
    if let Err(violation) = validate_wire_value(E::RESPONSE_SCHEMA, &json) {
        return Err(hosted_contract_error::<E>(
            HostedContractFailure::ResponseDecoding,
            Some(violation.path()),
        ));
    }
    serde_json::from_value(json)
        .map_err(|_| hosted_contract_error::<E>(HostedContractFailure::ResponseDecoding, None))
}

// Convex represents every number as an f64, so an integer wire field arrives as
// an integral float (e.g. `7.0`). serde will not decode that into `u64`, so
// rewrite integral floats to integer JSON numbers before decoding; genuine
// fractional values are left untouched and fail decoding as before. Mirrors the
// float handling the hand-written `value_u64` parser performed.
fn normalize_integral_floats(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Number(number) => normalize_number(number),
        JsonValue::Array(items) => {
            JsonValue::Array(items.into_iter().map(normalize_integral_floats).collect())
        }
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, normalize_integral_floats(value)))
                .collect(),
        ),
        other => other,
    }
}

fn normalize_number(number: serde_json::Number) -> JsonValue {
    if number.as_i64().is_some() || number.as_u64().is_some() {
        return JsonValue::Number(number);
    }
    match number.as_f64() {
        Some(float) if float.fract() == 0.0 && (0.0..=SAFE_INTEGER_MAX).contains(&float) => {
            JsonValue::Number((float as u64).into())
        }
        Some(float) if float.fract() == 0.0 && (-SAFE_INTEGER_MAX..0.0).contains(&float) => {
            JsonValue::Number((float as i64).into())
        }
        _ => JsonValue::Number(number),
    }
}

// 2^53 - 1: the largest integer an f64 represents exactly and the bound the
// generator enforces on safe-integer wire fields.
const SAFE_INTEGER_MAX: f64 = 9_007_199_254_740_991.0;

fn hosted_contract_error<E: HostedEndpoint>(
    failure: HostedContractFailure,
    field_path: Option<&str>,
) -> ControlPlaneError {
    // The field path is a structural locator, never the offending value, so it
    // is safe to surface and cannot leak a secret payload.
    let detail = field_path.map_or(String::new(), |path| format!(" (field `{path}`)"));
    ControlPlaneError::Storage(format!(
        "hosted endpoint `{}` (`{}`): {}{detail}",
        E::ID,
        E::CONVEX_FUNCTION,
        failure.message()
    ))
}

pub(super) fn unwrap_function_result(result: FunctionResult) -> ControlPlaneResult<Value> {
    match result {
        FunctionResult::Value(value) => Ok(value),
        FunctionResult::ErrorMessage(message) => Err(ControlPlaneError::Storage(format!(
            "Convex function failed: {message}"
        ))),
        FunctionResult::ConvexError(error) => Err(parse_convex_error(error)),
    }
}

fn parse_convex_error(error: ConvexError) -> ControlPlaneError {
    let Value::Object(payload) = &error.data else {
        return ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            message: format!("{error:?}"),
        };
    };
    let Some(Value::String(code)) = payload.get("code") else {
        return ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            message: format!("{error:?}"),
        };
    };
    let Some(Value::String(message)) = payload.get("message") else {
        return ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            message: format!("{error:?}"),
        };
    };
    ControlPlaneError::Rejected {
        code: RejectionCode::from_wire(code),
        message: message.clone(),
    }
}

pub(super) fn map_convex_error(error: impl fmt::Display) -> ControlPlaneError {
    ControlPlaneError::Transport {
        detail: error.to_string(),
    }
}

pub(super) fn add_field_context(
    error: ControlPlaneError,
    field: &'static str,
) -> ControlPlaneError {
    match error {
        ControlPlaneError::Storage(message) => shape_error(format!("field `{field}`: {message}")),
        error => error,
    }
}

pub(super) fn shape_error(message: impl Into<String>) -> ControlPlaneError {
    ControlPlaneError::Storage(message.into())
}
