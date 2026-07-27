use serde::{Deserialize, Serialize};

/// Declares an opaque identifier newtype.
///
/// The generated type deliberately has no public field, no `Deref<Target = str>`,
/// no `AsRef<str>`, and no `From<{&str, String}>`: those made the identifiers
/// structurally interchangeable with the raw strings they exist to replace, so
/// two different IDs could be swapped at a call site without a compile error.
/// Crossing between an ID and a string is always explicit: `new`, `as_str`, or
/// `into_string`.
macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(AccountId);
id_type!(DeviceId);
id_type!(DeviceApprovalRequestId);
id_type!(EncryptedDeviceGrantId);
id_type!(RecoveryEnvelopeId);
id_type!(WorkOsUserId);
id_type!(WorkOsOrganizationId);
id_type!(ProjectId);
id_type!(SnapshotId);
id_type!(ContentId);
id_type!(LeaseId);
id_type!(WorkViewId);
id_type!(EventId);
id_type!(PolicyVersion);
id_type!(EnvRecordId);
id_type!(BootstrapSessionId);

#[cfg(test)]
mod tests {
    use super::WorkspaceId;

    #[test]
    fn id_newtypes_serialize_transparently() {
        assert_eq!(
            serde_json::to_string(&WorkspaceId::new("w-1")).expect("workspace ID serializes"),
            "\"w-1\""
        );
        assert_eq!(
            serde_json::from_str::<WorkspaceId>("\"w-1\"").expect("workspace ID deserializes"),
            WorkspaceId::new("w-1")
        );
    }
}
