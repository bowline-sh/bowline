use super::*;

pub(super) type ConvexArgs = BTreeMap<String, Value>;

pub(super) async fn convex_rpc_timeout<F>(future: F) -> ControlPlaneResult<Value>
where
    F: Future<Output = ControlPlaneResult<Value>>,
{
    tokio::time::timeout(CONVEX_RPC_TIMEOUT, future)
        .await
        .map_err(|_| ControlPlaneError::Limited {
            capability: HOSTED_CAPABILITY,
            reason: "hosted Convex request timed out",
        })?
}

pub(super) fn args<const N: usize>(entries: [(&'static str, Value); N]) -> ConvexArgs {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

pub(super) fn unwrap_function_result(result: FunctionResult) -> ControlPlaneResult<Value> {
    match result {
        FunctionResult::Value(value) => Ok(value),
        FunctionResult::ErrorMessage(message) => Err(ControlPlaneError::Storage(format!(
            "Convex function failed: {message}"
        ))),
        FunctionResult::ConvexError(error) => Err(ControlPlaneError::Storage(format!(
            "Convex function failed: {error:?}"
        ))),
    }
}

pub(super) fn map_convex_error(error: impl fmt::Display) -> ControlPlaneError {
    ControlPlaneError::Storage(format!("Convex client failed: {error}"))
}

pub(super) fn value_object(value: &Value) -> ControlPlaneResult<&BTreeMap<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(shape_error("expected Convex object")),
    }
}

pub(super) fn required_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> Result<&'a Value, CompareAndSwapError> {
    object.get(field).ok_or(CompareAndSwapError::Unsupported {
        capability: HOSTED_CAPABILITY,
        reason: "Convex result was missing a required field",
    })
}

pub(super) fn string_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<String> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(shape_error("expected Convex string field")),
    }
}

pub(super) fn value_string(value: &Value) -> ControlPlaneResult<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        _ => Err(shape_error("expected Convex string value")),
    }
}

pub(super) fn optional_string_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<Option<String>> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        _ => Err(shape_error("expected optional Convex string field")),
    }
}

pub(super) fn u64_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<u64> {
    match object.get(field) {
        Some(value) => value_u64(value),
        None => Err(shape_error("expected Convex numeric field")),
    }
}

pub(super) fn value_u64(value: &Value) -> ControlPlaneResult<u64> {
    match value {
        Value::Int64(value) if *value >= 0 => Ok(*value as u64),
        Value::Float64(value) if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 => {
            Ok(*value as u64)
        }
        _ => Err(shape_error("expected non-negative integer-valued number")),
    }
}

pub(super) fn bool_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<bool> {
    match object.get(field) {
        Some(Value::Boolean(value)) => Ok(*value),
        _ => Err(shape_error("expected Convex boolean field")),
    }
}

pub(super) fn array_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<&'a Vec<Value>> {
    match object.get(field) {
        Some(Value::Array(value)) => Ok(value),
        _ => Err(shape_error("expected Convex array field")),
    }
}

pub(super) fn shape_error(message: impl Into<String>) -> ControlPlaneError {
    ControlPlaneError::Storage(message.into())
}
