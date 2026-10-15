//! Infrastructure-independent domain types for FigCraft Cloud.
//!
//! Infrastructure adapters use these distinct types instead of passing interchangeable UUIDs.

use std::fmt;
use std::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub use uuid::Uuid;

/// A supplied cloud identifier was not a canonical, non-nil UUID.
///
/// The error deliberately does not retain or echo the rejected input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseCloudIdError;

impl fmt::Display for ParseCloudIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid cloud identifier")
    }
}

impl std::error::Error for ParseCloudIdError {}

fn parse_uuid(input: &str) -> Result<Uuid, ParseCloudIdError> {
    let uuid = Uuid::parse_str(input).map_err(|_| ParseCloudIdError)?;
    if uuid.is_nil() || uuid.hyphenated().to_string() != input {
        return Err(ParseCloudIdError);
    }
    Ok(uuid)
}

struct CloudIdVisitor;

impl<'de> Visitor<'de> for CloudIdVisitor {
    type Value = Uuid;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical, non-nil UUID")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        parse_uuid(value).map_err(E::custom)
    }
}

macro_rules! define_cloud_ids {
    ($($(#[$meta:meta])* $name:ident;)+) => {
        $(
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
            pub struct $name(Uuid);

            impl $name {
                /// Generate a random UUIDv4 identifier.
                #[must_use]
                pub fn random() -> Self {
                    Self(Uuid::new_v4())
                }

                /// Return the underlying UUID.
                #[must_use]
                pub const fn as_uuid(self) -> Uuid {
                    self.0
                }
            }

            impl TryFrom<Uuid> for $name {
                type Error = ParseCloudIdError;

                fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                    if value.is_nil() {
                        Err(ParseCloudIdError)
                    } else {
                        Ok(Self(value))
                    }
                }
            }

            impl FromStr for $name {
                type Err = ParseCloudIdError;

                fn from_str(input: &str) -> Result<Self, Self::Err> {
                    parse_uuid(input).map(Self)
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(formatter, "{}", self.0.hyphenated())
                }
            }

            impl Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: Serializer,
                {
                    serializer.collect_str(self)
                }
            }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: Deserializer<'de>,
                {
                    deserializer.deserialize_str(CloudIdVisitor).map(Self)
                }
            }
        )+
    };
}

define_cloud_ids! {
    /// Identifies a tenant in every deployment mode.
    TenantId;
    /// Identifies a workspace within a tenant.
    WorkspaceId;
    /// Identifies a project within a workspace.
    ProjectId;
    /// Identifies one cloud document across browser, desktop, API and realtime surfaces.
    CloudDocumentId;
    /// Identifies a user or service principal.
    PrincipalId;
    /// Identifies immutable object metadata.
    ObjectId;
    /// Identifies a durable asynchronous job.
    JobId;
    /// Correlates one request across service boundaries.
    RequestId;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "018f2f35-8dd8-7c25-8f3f-3a728cbd632a";

    #[test]
    fn every_identifier_uses_the_canonical_uuid_wire_shape() {
        macro_rules! assert_id {
            ($name:ident) => {{
                let id: $name = ID.parse().unwrap();
                assert_eq!(id.to_string(), ID);
                assert_eq!(id.as_uuid().hyphenated().to_string(), ID);
            }};
        }

        assert_id!(TenantId);
        assert_id!(WorkspaceId);
        assert_id!(ProjectId);
        assert_id!(CloudDocumentId);
        assert_id!(PrincipalId);
        assert_id!(ObjectId);
        assert_id!(JobId);
        assert_id!(RequestId);
    }

    #[test]
    fn generated_identifiers_are_random_uuid_v4_values() {
        let first = TenantId::random();
        let second = TenantId::random();
        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 4);
        assert_eq!(second.as_uuid().get_version_num(), 4);
    }
}
