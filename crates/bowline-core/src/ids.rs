use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
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
id_type!(ManifestId);
id_type!(PackId);
id_type!(ContentId);
id_type!(LeaseId);
id_type!(WorkViewId);
id_type!(EventId);
id_type!(PolicyVersion);
id_type!(EnvRecordId);
