use crate::{ControlPlaneError, ControlPlaneResult};

pub fn is_opaque_object_key(object_key: &str) -> bool {
    bowline_storage::ObjectKey::new(object_key).is_ok()
}

pub(crate) fn validate_object_key(object_key: &str) -> ControlPlaneResult<()> {
    match bowline_storage::ObjectKey::new(object_key) {
        Ok(_) => Ok(()),
        Err(_) => Err(ControlPlaneError::InvalidObjectKey {
            reason: "object keys must be generated opaque pack, manifest, or overlay keys",
        }),
    }
}
