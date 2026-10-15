//! Infrastructure-independent domain types for FigCraft Cloud.
//!
//! Infrastructure adapters use these distinct types instead of passing interchangeable UUIDs.

use std::collections::BTreeSet;
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

/// How one cloud installation resolves tenants and authentication.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum DeploymentMode {
    /// One configured tenant owns every resource in this installation.
    #[serde(rename = "single")]
    SingleTenant,
    /// Authenticated principals can select from multiple authorized tenants.
    #[serde(rename = "multi")]
    MultiTenant,
    /// A registered host maps external workspaces into FigCraft tenants.
    #[serde(rename = "embedded")]
    Embedded,
}

/// A principal's role at a tenant, team, workspace or document boundary.
///
/// `TeamManager` is intentionally not ordered above document roles: managing a team does not grant
/// document editing access. Authorization resolves the relevant grants instead of comparing enum
/// discriminants.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    TenantOwner,
    TenantAdmin,
    TeamManager,
    WorkspaceAdmin,
    Editor,
    Commenter,
    Viewer,
}

/// Sensitive capabilities that require an explicit grant in addition to a broad role.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ManageBilling,
    ManageProviderKeys,
    UseSupportAccess,
    ExportAudit,
    TransferTenantOwnership,
    DeleteTenant,
}

/// Shared lifecycle states used by tenant-owned resources.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Pending,
    Active,
    Suspended,
    Archived,
    Deleting,
    Deleted,
}

impl ResourceState {
    /// Whether normal resource operations may proceed before finer authorization checks.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// A non-negative usage or limit measured in the unit named by its surrounding policy.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuotaValue(u64);

impl QuotaValue {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Add usage without wrapping a counter controlled by untrusted activity.
    #[must_use]
    pub const fn checked_add(self, additional: Self) -> Option<Self> {
        match self.0.checked_add(additional.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// A plan or resource limit. Zero is a valid limited value that disables an operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum QuotaLimit {
    Unlimited,
    Limited(QuotaValue),
}

impl QuotaLimit {
    /// Whether adding `requested` to `used` remains within this limit.
    #[must_use]
    pub const fn allows(self, used: QuotaValue, requested: QuotaValue) -> bool {
        match self {
            Self::Unlimited => true,
            Self::Limited(limit) => match used.checked_add(requested) {
                Some(total) => total.0 <= limit.0,
                None => false,
            },
        }
    }
}

/// Authenticated tenant and principal facts resolved before repositories or handlers run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantContext {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    deployment: DeploymentMode,
    role: Role,
    permissions: BTreeSet<Permission>,
}

impl TenantContext {
    #[must_use]
    pub fn new(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        deployment: DeploymentMode,
        role: Role,
        permissions: impl IntoIterator<Item = Permission>,
    ) -> Self {
        Self { tenant_id, principal_id, deployment, role, permissions: permissions.into_iter().collect() }
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn deployment(&self) -> DeploymentMode {
        self.deployment
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub fn permissions(&self) -> &BTreeSet<Permission> {
        &self.permissions
    }

    #[must_use]
    pub fn has_permission(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }
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

    #[test]
    fn tenant_context_keeps_role_and_explicit_permissions_separate() {
        let tenant_id = TenantId::random();
        let principal_id = PrincipalId::random();
        let context = TenantContext::new(
            tenant_id,
            principal_id,
            DeploymentMode::MultiTenant,
            Role::TeamManager,
            [Permission::ExportAudit, Permission::ExportAudit],
        );

        assert_eq!(context.tenant_id(), tenant_id);
        assert_eq!(context.principal_id(), principal_id);
        assert_eq!(context.deployment(), DeploymentMode::MultiTenant);
        assert_eq!(context.role(), Role::TeamManager);
        assert!(context.has_permission(Permission::ExportAudit));
        assert!(!context.has_permission(Permission::DeleteTenant));
        assert_eq!(context.permissions().len(), 1);
    }

    #[test]
    fn resource_and_quota_values_fail_closed() {
        assert!(ResourceState::Active.is_active());
        assert!(!ResourceState::Suspended.is_active());
        assert!(QuotaLimit::Limited(QuotaValue::new(10)).allows(QuotaValue::new(7), QuotaValue::new(3)));
        assert!(!QuotaLimit::Limited(QuotaValue::new(10)).allows(QuotaValue::new(7), QuotaValue::new(4)));
        assert!(!QuotaLimit::Limited(QuotaValue::new(u64::MAX)).allows(QuotaValue::new(u64::MAX), QuotaValue::new(1)));
        assert!(QuotaLimit::Unlimited.allows(QuotaValue::new(u64::MAX), QuotaValue::new(1)));
    }
}
