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

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.as_str()
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                self.as_str() == other.as_str()
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
id_type!(ManifestDigest);
id_type!(NamespacePageId);
id_type!(ContentLayoutId);
id_type!(SegmentPageId);
id_type!(PackId);
id_type!(ContentId);
id_type!(LeaseId);
id_type!(WorkViewId);
id_type!(EventId);
id_type!(PolicyVersion);
id_type!(EnvRecordId);
id_type!(BootstrapSessionId);
id_type!(ConflictId);

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
