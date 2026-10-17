//! Relay admission by effective document role (S4-021 relay action matrix): the
//! [`PrincipalAdmission`] the cloud binary gives the collaboration backend so a signed-in
//! principal joins a document's room by their role rather than by a share link.

use std::time::Duration;

use figcraft_cloud_auth::authz::Action;
use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_relay::admission::{AdmissionFuture, AdmissionUnavailable, PrincipalAdmission, RelayAccess};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::effective_roles::{ResourceAccess, resolve_document_access};
use sqlx::PgPool;

use super::{Kind, allows};

/// Longest one admission decision may take.
pub(crate) const ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The relay action matrix over resolved facts: join (`document.join_realtime`) and edit
/// (`document.edit`) decided by the authorization policy; a missing document is refused.
pub(crate) fn relay_access(access: Option<&ResourceAccess>) -> RelayAccess {
    access.map_or(RelayAccess::Refused, |access| {
        RelayAccess::from_decisions(
            allows(access, Kind::Document, Action::DocumentJoinRealtime),
            allows(access, Kind::Document, Action::DocumentEdit),
        )
    })
}

/// Decides admission from `effective_roles` in the principal's tenant.
#[derive(Clone)]
pub(crate) struct DocumentRoleAdmission {
    pool: PgPool,
    timeout: Duration,
}

impl DocumentRoleAdmission {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool, timeout: ADMISSION_TIMEOUT }
    }

    async fn decide(&self, tenant: TenantId, principal: PrincipalId, document: CloudDocumentId) -> Result<RelayAccess, AdmissionUnavailable> {
        let mut transaction = begin_tenant_transaction(&self.pool, tenant).await.map_err(|_| AdmissionUnavailable)?;
        let access = resolve_document_access(&mut transaction, principal, document).await.map_err(|_| AdmissionUnavailable)?;
        Ok(relay_access(access.as_ref()))
    }
}

impl PrincipalAdmission for DocumentRoleAdmission {
    fn admit<'a>(&'a self, tenant: TenantId, principal: PrincipalId, document: CloudDocumentId) -> AdmissionFuture<'a> {
        Box::pin(async move { tokio::time::timeout(self.timeout, self.decide(tenant, principal, document)).await.map_err(|_| AdmissionUnavailable)? })
    }
}
