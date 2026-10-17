//! Workspace policy and workspace lifecycle contracts (S6-047, S6-056).
//!
//! The product's "workspace" (a company) is the `tenants` table. Two small contracts live here so
//! the sign-in code, the API, the relay and the UI all read the same shapes:
//!
//! * [`WorkspacePolicy`]: what the owner and admins can set per workspace. Stored as JSON in the
//!   tenant row (S6-056) and enforced on the server.
//! * [`serves_workspace_traffic`] and [`lifecycle_transition_allowed`]: what each lifecycle state
//!   ([`ResourceState`]) allows. The lifecycle enum is the existing `tenants.state`: `pending`,
//!   `active`, `suspended`, `archived`, `deleting` (pending deletion) and `deleted`.
//!
//! # Lifecycle policy (documented decision)
//!
//! Only an `active` workspace serves workspace traffic. In every other state, every workspace
//! surface is refused (`409 resource_inactive` on the API, a close on realtime connections, no
//! webhook deliveries, no share-link or embed sessions) and live sessions are ended promptly
//! through the revocation machinery. No state deletes data: `suspended` and `archived` are
//! reversible by the platform, `deleting` only by a platform operator before the grace period
//! ends. Read-only export for an archived workspace is not offered yet.

use serde::{Deserialize, Serialize};

use crate::ResourceState;

/// A way to sign in to a workspace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInMethod {
    /// Built-in accounts (login plus password).
    Password,
    /// The workspace's OpenID Connect provider.
    Oidc,
}

/// Who may invite people into the workspace or a team.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitePolicy {
    /// Only the owner and workspace admins.
    AdminsOnly,
    /// Admins, and team leads for their own team.
    TeamLeads,
    /// Any editor or above may invite to teams they belong to.
    Members,
}

/// What share links (anyone-with-the-link access) may do in this workspace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareLinkPolicy {
    Disabled,
    ViewOnly,
    ViewAndEdit,
}

/// Per-workspace settings, editable only by the owner and workspace admins.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicy {
    /// Never empty. Sign-in with a method not listed is refused for this workspace.
    pub allowed_sign_in_methods: Vec<SignInMethod>,
    pub who_may_invite: InvitePolicy,
    /// Whether people outside the workspace's members may be given guest (viewer or commenter)
    /// access to single documents.
    pub guest_access: bool,
    pub share_links: ShareLinkPolicy,
}

impl Default for WorkspacePolicy {
    /// Both sign-in methods, team leads may invite, no guests, share links as before this setting
    /// existed (view and edit).
    fn default() -> Self {
        Self {
            allowed_sign_in_methods: vec![SignInMethod::Password, SignInMethod::Oidc],
            who_may_invite: InvitePolicy::TeamLeads,
            guest_access: false,
            share_links: ShareLinkPolicy::ViewAndEdit,
        }
    }
}

impl WorkspacePolicy {
    /// A policy is valid when at least one sign-in method remains and each is listed once.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let methods = &self.allowed_sign_in_methods;
        match methods.as_slice() {
            [_] => true,
            [a, b] => a != b,
            _ => false,
        }
    }

    #[must_use]
    pub fn allows_sign_in(&self, method: SignInMethod) -> bool {
        self.allowed_sign_in_methods.contains(&method)
    }
}

/// Whether workspace members, service accounts, share links, embed and host tokens, webhooks and
/// the relay are served in this state. Only `active` is.
#[must_use]
pub const fn serves_workspace_traffic(state: ResourceState) -> bool {
    matches!(state, ResourceState::Active)
}

/// Whether the platform may move a workspace from `from` to `to`.
///
/// `pending` -> `active` (owner accepted); `active` <-> `suspended`; `active` or `suspended` ->
/// `archived`; `archived` -> `active` (restore); `archived` or `suspended` -> `deleting`;
/// `deleting` -> `archived` (cancel before the grace period ends). `deleted` is terminal and set
/// only by the deletion job, never by a request.
#[must_use]
pub const fn lifecycle_transition_allowed(from: ResourceState, to: ResourceState) -> bool {
    use ResourceState::{Active, Archived, Deleting, Pending, Suspended};
    matches!(
        (from, to),
        (Pending, Active)
            | (Active, Suspended)
            | (Suspended, Active)
            | (Active | Suspended, Archived)
            | (Archived, Active)
            | (Archived | Suspended, Deleting)
            | (Deleting, Archived)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ResourceState::{Active, Archived, Deleted, Deleting, Pending, Suspended};

    #[test]
    fn only_active_workspaces_serve_traffic() {
        for state in [Pending, Suspended, Archived, Deleting, Deleted] {
            assert!(!serves_workspace_traffic(state));
        }
        assert!(serves_workspace_traffic(Active));
    }

    #[test]
    fn transitions_never_leave_deleted_or_skip_the_grace_state() {
        for to in [Pending, Active, Suspended, Archived, Deleting, Deleted] {
            assert!(!lifecycle_transition_allowed(Deleted, to));
            assert!(!lifecycle_transition_allowed(to, Deleted));
        }
        assert!(!lifecycle_transition_allowed(Active, Deleting));
        assert!(lifecycle_transition_allowed(Suspended, Active));
        assert!(lifecycle_transition_allowed(Deleting, Archived));
    }

    #[test]
    fn policy_rejects_empty_and_duplicate_methods_and_unknown_fields() {
        assert!(WorkspacePolicy::default().is_valid());
        let mut policy = WorkspacePolicy::default();
        policy.allowed_sign_in_methods.clear();
        assert!(!policy.is_valid());
        policy.allowed_sign_in_methods = vec![SignInMethod::Oidc, SignInMethod::Oidc];
        assert!(!policy.is_valid());
        policy.allowed_sign_in_methods = vec![SignInMethod::Oidc];
        assert!(policy.is_valid() && policy.allows_sign_in(SignInMethod::Oidc) && !policy.allows_sign_in(SignInMethod::Password));
        let json = serde_json::to_string(&WorkspacePolicy::default()).unwrap();
        assert_eq!(serde_json::from_str::<WorkspacePolicy>(&json).unwrap(), WorkspacePolicy::default());
        assert!(serde_json::from_str::<WorkspacePolicy>(&json.replace("guest_access", "guests")).is_err());
    }
}
