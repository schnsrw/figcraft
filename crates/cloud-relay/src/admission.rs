//! Relay admission by document role (S4-019, S4-021).
//!
//! [`crate::collab_backend::CloudBackend::admit`] admits a hello by share link today. A hello can
//! also come from a signed-in principal (the gateway authenticates the WebSocket upgrade and the
//! browser presents a short-lived token naming the principal); such a hello is admitted by the
//! principal's *effective document role*, not by a link. The decision itself is the
//! authorization policy's (`figcraft_cloud_auth::authz::decide` for `document.join_realtime` and
//! `document.edit`), which this crate cannot depend on (layering: L8 crates only reach storage),
//! so the cloud binary implements [`PrincipalAdmission`] and the backend maps the two decisions
//! to an [`Access`] through [`RelayAccess::from_decisions`].
//!
//! The mapping is the relay action matrix: a principal who may join and edit joins with
//! [`Access::Edit`]; one who may join but not edit (viewer, commenter, or any role on an archived
//! document) joins with [`Access::View`]; anyone else is refused with the same message as an
//! unknown link.

use std::future::Future;
use std::pin::Pin;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::share_links::ShareRole;
use figcraft_collab::message::Access;

/// What a hello is admitted with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayAccess {
    /// Join and submit batches.
    Edit,
    /// Join read-only.
    View,
    /// Not admitted (reads exactly like an unknown link).
    Refused,
}

impl RelayAccess {
    /// The relay action matrix: `join` is the decision for `document.join_realtime`, `edit` for
    /// `document.edit`. Editing without the right to join is refused (deny by default).
    #[must_use]
    pub const fn from_decisions(join: bool, edit: bool) -> Self {
        match (join, edit) {
            (true, true) => Self::Edit,
            (true, false) => Self::View,
            (false, _) => Self::Refused,
        }
    }

    /// What a live share link of `role` admits with.
    #[must_use]
    pub const fn for_share_role(role: ShareRole) -> Self {
        match role {
            ShareRole::Editor => Self::Edit,
            ShareRole::Viewer => Self::View,
        }
    }

    /// The hub access, `None` when refused.
    #[must_use]
    pub const fn access(self) -> Option<Access> {
        match self {
            Self::Edit => Some(Access::Edit),
            Self::View => Some(Access::View),
            Self::Refused => None,
        }
    }
}

/// Why a principal's admission couldn't be decided (storage unreachable or timed out). Answered
/// with the relay's "unavailable" message, never with a refusal reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionUnavailable;

/// The future a [`PrincipalAdmission`] returns.
pub type AdmissionFuture<'a> = Pin<Box<dyn Future<Output = Result<RelayAccess, AdmissionUnavailable>> + Send + 'a>>;

/// Decides a signed-in principal's access to a document's realtime room from their effective
/// document role. Implementations resolve roles inside a tenant transaction, bound their work and
/// answer [`RelayAccess::Refused`] for a document that does not exist in that tenant.
pub trait PrincipalAdmission: Send + Sync + 'static {
    fn admit<'a>(&'a self, tenant: TenantId, principal: PrincipalId, document: CloudDocumentId) -> AdmissionFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_edit_decisions_map_to_edit_view_or_refused() {
        assert_eq!(RelayAccess::from_decisions(true, true), RelayAccess::Edit);
        assert_eq!(RelayAccess::from_decisions(true, false), RelayAccess::View);
        assert_eq!(RelayAccess::from_decisions(false, false), RelayAccess::Refused);
        // An edit right without the join right never admits.
        assert_eq!(RelayAccess::from_decisions(false, true), RelayAccess::Refused);
        assert_eq!(RelayAccess::Edit.access(), Some(Access::Edit));
        assert_eq!(RelayAccess::View.access(), Some(Access::View));
        assert_eq!(RelayAccess::Refused.access(), None);
        assert_eq!(RelayAccess::for_share_role(ShareRole::Editor), RelayAccess::Edit);
        assert_eq!(RelayAccess::for_share_role(ShareRole::Viewer), RelayAccess::View);
    }
}
