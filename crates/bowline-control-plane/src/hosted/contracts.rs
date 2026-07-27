use serde::{Serialize, de::DeserializeOwned};

use super::ConvexRpcKind;

pub(crate) trait HostedEndpoint {
    const ID: &'static str;
    const CONVEX_FUNCTION: &'static str;
    const KIND: ConvexRpcKind;
    /// Name of the generated wire declaration validated for the request.
    const REQUEST_SCHEMA: &'static str;
    /// Name of the generated wire declaration validated for the response.
    const RESPONSE_SCHEMA: &'static str;

    type Request: Serialize;
    type Response: DeserializeOwned;
}
