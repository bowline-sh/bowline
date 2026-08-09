//! Production identity validation for the engine/transport boundary.

use std::io;

use bowline_local::sync::manifest_engine::EngineContext;
use bowline_storage::workspace_id_hash;

pub(super) fn validate_engine_context_identity(context: &EngineContext) -> io::Result<()> {
    if context.crypto.workspace_id_hash() != workspace_id_hash(context.workspace_identity.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest engine crypto does not match its workspace identity",
        ));
    }
    Ok(())
}
