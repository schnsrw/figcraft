//! Edit attribution that tells a person from a team (S6-039).
//!
//! Every committed batch records its principal (`actor_principal_id`) and, now, how that principal
//! held access to the document ([`Via`]): directly as a member, through a team grant, through a share
//! link, or as a service account. History reads "Ada, through the Design team".
//!
//! [`access_via`] decides it at admission, with the same role sources the authorization policy uses
//! ([`crate::effective_roles`]): when any *direct* source supplies the effective role (tenant role,
//! workspace membership, document membership) the access is `Member`; only when the strongest role
//! comes exclusively from team grants is it `Team`, naming the (lowest-id) team that granted it.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Uuid};
use std::fmt;

use crate::TenantTransaction;
use crate::effective_roles::{RoleSource, resolve_document_access};

/// How access to a document was held.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Via {
    /// Directly: a tenant, workspace or document membership of the person.
    Member,
    /// Through a team grant; the id is the team.
    Team(Uuid),
    /// Through a share link; the id is the link.
    ShareLink(Uuid),
    /// As a service account; the id is the service account.
    ServiceAccount(Uuid),
}

impl Via {
    /// The stored `via_kind`.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Team(_) => "team",
            Self::ShareLink(_) => "share_link",
            Self::ServiceAccount(_) => "service_account",
        }
    }

    /// The stored `via_id`.
    #[must_use]
    pub const fn id(self) -> Option<Uuid> {
        match self {
            Self::Member => None,
            Self::Team(id) | Self::ShareLink(id) | Self::ServiceAccount(id) => Some(id),
        }
    }

    /// Rebuild from stored columns; `None` for NULL or anything inconsistent.
    #[must_use]
    pub fn from_parts(kind: Option<&str>, id: Option<Uuid>) -> Option<Self> {
        match (kind?, id) {
            ("member", None) => Some(Self::Member),
            ("team", Some(id)) => Some(Self::Team(id)),
            ("share_link", Some(id)) => Some(Self::ShareLink(id)),
            ("service_account", Some(id)) => Some(Self::ServiceAccount(id)),
            _ => None,
        }
    }
}

/// Fixed, non-leaking categories for attribution lookups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttributionError {
    Database,
}

impl fmt::Display for AttributionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "attribution error: {self:?}")
    }
}

impl std::error::Error for AttributionError {}

/// How `principal_id` holds access to `document_id` in the transaction's tenant. `None` when they
/// hold none (the caller then records nothing rather than guessing).
pub async fn access_via(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    document_id: CloudDocumentId,
) -> Result<Option<Via>, AttributionError> {
    let access = resolve_document_access(transaction, principal_id, document_id).await.map_err(|_| AttributionError::Database)?;
    let Some(access) = access.filter(|access| access.role.is_some()) else {
        return Ok(None);
    };
    let direct = access.sources.iter().any(|source| matches!(source, RoleSource::Tenant | RoleSource::WorkspaceMember | RoleSource::DocumentMember));
    if direct || access.sources.is_empty() {
        return Ok(Some(Via::Member));
    }
    // Only team grants supply the role: name a team that grants it.
    let team: Option<String> = sqlx::query_scalar(concat!(
        "SELECT g.team_id::text FROM (",
        "  SELECT team_id FROM workspace_team_grants WHERE tenant_id = $1::uuid ",
        "    AND workspace_id = (SELECT workspace_id FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid) ",
        "  UNION ALL SELECT team_id FROM document_team_grants WHERE tenant_id = $1::uuid AND document_id = $2::uuid",
        ") g JOIN teams t ON t.tenant_id = $1::uuid AND t.id = g.team_id AND t.state = 'active' ",
        "JOIN team_members tm ON tm.tenant_id = $1::uuid AND tm.team_id = g.team_id AND tm.state = 'active' AND tm.principal_id = $3::uuid ",
        "ORDER BY g.team_id LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(principal_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| AttributionError::Database)?;
    Ok(Some(match team.and_then(|team| Uuid::parse_str(&team).ok()) {
        Some(team) => Via::Team(team),
        None => Via::Member,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_parts_round_trip_and_inconsistent_ones_are_unknown() {
        let id = Uuid::new_v4();
        for via in [Via::Member, Via::Team(id), Via::ShareLink(id), Via::ServiceAccount(id)] {
            assert_eq!(Via::from_parts(Some(via.kind()), via.id()), Some(via));
        }
        assert_eq!(Via::from_parts(None, None), None);
        assert_eq!(Via::from_parts(Some("team"), None), None);
        assert_eq!(Via::from_parts(Some("member"), Some(id)), None);
        assert_eq!(Via::from_parts(Some("other"), None), None);
    }
}
