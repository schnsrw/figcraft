//! Deny-by-default authorization policy (S4-012, S4-008, S4-010, S4-011).
//!
//! This module is pure: it never reads storage. Callers resolve the facts about a principal
//! (`figcraft_cloud_storage::effective_roles` computes them from tenant membership, direct
//! workspace/document membership and team grants) and pass them in as a [`Subject`] and a
//! [`Resource`]; [`decide`] returns a [`Decision`] with a stable, non-leaking reason.
//!
//! # Rules
//!
//! 1. An action belongs to exactly one resource scope ([`Action::scope`]); asking about it on a
//!    resource of another kind is denied.
//! 2. The principal must be active, the tenant must be `active`, and the principal must hold an
//!    active tenant membership. Anything else denies every action, including reads.
//! 3. The resource must be `active`, except that an `archived` resource still allows its read
//!    actions (`view`, `export`), `restore` and `delete`, and a `deleting` (soft-deleted) resource
//!    allows only `restore`. `pending`, `suspended` and `deleted` resources allow nothing.
//! 4. The role that applies to the scope must allow the action in the [`role_allows`] matrix.
//!    For tenant actions that is the tenant role; for team actions the team-member role (tenant
//!    owners and admins administer every team); for workspace, project and document actions the
//!    effective resource role. Tenant owners and admins rank as workspace admins on resources;
//!    `team_manager` grants nothing on resources (managing a team never upgrades content access).
//! 5. Sensitive actions ([`Action::sensitive_permission`]) additionally require the explicit
//!    [`SensitivePermission`] grant. Roles never imply one, not even `tenant_owner`: billing,
//!    provider keys, support-access grants, tenant/audit export and tenant deletion each need a
//!    `member_permissions` row as well as a sufficient role. Granting those permissions is itself
//!    a tenant-owner action that needs no sensitive grant, so a new owner can grant themselves.
//! 6. Actions the schema cannot yet support safely ([`Action::TenantTransferOwnership`], which
//!    has no `member_permissions` value) are denied for everyone.

use std::collections::BTreeSet;

use figcraft_cloud_domain::{ResourceState, Role};

/// Explicit grants stored in `member_permissions.permission`. Never implied by a role.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SensitivePermission {
    Billing,
    ProviderKeys,
    Support,
    Export,
    Deletion,
}

impl SensitivePermission {
    pub const ALL: [Self; 5] = [Self::Billing, Self::ProviderKeys, Self::Support, Self::Export, Self::Deletion];

    /// The `member_permissions.permission` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Billing => "billing",
            Self::ProviderKeys => "provider_keys",
            Self::Support => "support",
            Self::Export => "export",
            Self::Deletion => "deletion",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|permission| permission.as_str() == value)
    }
}

/// The kind of resource an action applies to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Scope {
    Tenant,
    Team,
    Workspace,
    Project,
    Document,
}

macro_rules! actions {
    ($($(#[$meta:meta])* $variant:ident => $id:literal, $scope:ident;)+) => {
        /// Every API and relay action that needs an authorization decision.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum Action {
            $($(#[$meta])* $variant,)+
        }

        impl Action {
            /// Every action, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// Stable dotted identifier (logs, audit `action` fields, API docs).
            #[must_use]
            pub const fn id(self) -> &'static str {
                match self {
                    $(Self::$variant => $id,)+
                }
            }

            /// The resource kind this action applies to.
            #[must_use]
            pub const fn scope(self) -> Scope {
                match self {
                    $(Self::$variant => Scope::$scope,)+
                }
            }
        }
    };
}

actions! {
    /// Read tenant name, plan and settings.
    TenantView => "tenant.view", Tenant;
    /// Change tenant policy and settings (not billing, keys or deletion).
    TenantUpdatePolicy => "tenant.update_policy", Tenant;
    /// Read the tenant audit log.
    TenantViewAudit => "tenant.view_audit", Tenant;
    /// Export the audit log. Sensitive: `export`.
    TenantExportAudit => "tenant.export_audit", Tenant;
    /// Export all tenant data. Sensitive: `export`.
    TenantExport => "tenant.export", Tenant;
    /// Delete the tenant. Sensitive: `deletion`.
    TenantDelete => "tenant.delete", Tenant;
    /// Transfer ownership. Denied until the schema has an explicit permission for it.
    TenantTransferOwnership => "tenant.transfer_ownership", Tenant;
    /// List tenant members.
    MemberView => "member.view", Tenant;
    /// Invite, change roles of, suspend and remove non-owner members.
    MemberManage => "member.manage", Tenant;
    /// Add, demote, suspend or remove tenant owners.
    MemberManageOwners => "member.manage_owners", Tenant;
    /// Grant or revoke sensitive permissions.
    MemberManagePermissions => "member.manage_permissions", Tenant;
    /// Change the plan and payment details. Sensitive: `billing`.
    BillingManage => "billing.manage", Tenant;
    /// Add, rotate or remove AI provider keys. Sensitive: `provider_keys`.
    ProviderKeysManage => "provider_keys.manage", Tenant;
    /// Grant time-boxed operator support access. Sensitive: `support`.
    SupportAccessGrant => "support.grant", Tenant;
    /// Create, rotate and disable service accounts.
    ServiceAccountManage => "service_account.manage", Tenant;
    /// Create a team.
    TeamCreate => "team.create", Tenant;
    /// Create a workspace.
    WorkspaceCreate => "workspace.create", Tenant;

    /// Read a team and its roster.
    TeamView => "team.view", Team;
    /// Manage the roster, invitations and team grants of a team.
    TeamManage => "team.manage", Team;

    /// Read a workspace and list its projects.
    WorkspaceView => "workspace.view", Workspace;
    /// Rename or change workspace settings.
    WorkspaceUpdate => "workspace.update", Workspace;
    /// Add, change and remove workspace members and team grants.
    WorkspaceManageMembers => "workspace.manage_members", Workspace;
    WorkspaceArchive => "workspace.archive", Workspace;
    WorkspaceRestore => "workspace.restore", Workspace;
    WorkspaceDelete => "workspace.delete", Workspace;
    /// Create a project inside the workspace.
    ProjectCreate => "project.create", Workspace;

    ProjectView => "project.view", Project;
    ProjectUpdate => "project.update", Project;
    ProjectArchive => "project.archive", Project;
    ProjectRestore => "project.restore", Project;
    ProjectDelete => "project.delete", Project;
    /// Create a document inside the project.
    DocumentCreate => "document.create", Project;

    /// Read document metadata and content (HTTP).
    DocumentView => "document.view", Document;
    /// Join the realtime room (relay admission, read-only for viewers).
    DocumentJoinRealtime => "document.join_realtime", Document;
    /// Publish presence and cursors in the realtime room.
    DocumentPresence => "document.presence", Document;
    /// Create, reply to and resolve comments.
    DocumentComment => "document.comment", Document;
    /// Submit design mutations (relay batches, HTTP edits).
    DocumentEdit => "document.edit", Document;
    /// Create a named version or snapshot.
    DocumentCreateVersion => "document.create_version", Document;
    /// Export the document (subject to plan export policy).
    DocumentExport => "document.export", Document;
    /// Manage direct members, team grants and share links of the document.
    DocumentShare => "document.share", Document;
    DocumentRename => "document.rename", Document;
    DocumentArchive => "document.archive", Document;
    DocumentRestore => "document.restore", Document;
    /// Soft-delete the document.
    DocumentDelete => "document.delete", Document;
}

impl Action {
    /// The explicit permission this action needs in addition to a role.
    #[must_use]
    pub const fn sensitive_permission(self) -> Option<SensitivePermission> {
        match self {
            Self::BillingManage => Some(SensitivePermission::Billing),
            Self::ProviderKeysManage => Some(SensitivePermission::ProviderKeys),
            Self::SupportAccessGrant => Some(SensitivePermission::Support),
            Self::TenantExport | Self::TenantExportAudit => Some(SensitivePermission::Export),
            Self::TenantDelete => Some(SensitivePermission::Deletion),
            _ => None,
        }
    }

    /// Read-only actions that remain available on an archived resource.
    const fn allowed_when_archived(self) -> bool {
        matches!(
            self,
            Self::WorkspaceView
                | Self::WorkspaceRestore
                | Self::WorkspaceDelete
                | Self::ProjectView
                | Self::ProjectRestore
                | Self::ProjectDelete
                | Self::DocumentView
                | Self::DocumentExport
                | Self::DocumentRestore
                | Self::DocumentDelete
        )
    }

    const fn allowed_when_deleting(self) -> bool {
        matches!(self, Self::WorkspaceRestore | Self::ProjectRestore | Self::DocumentRestore)
    }

    const fn unsupported(self) -> bool {
        matches!(self, Self::TenantTransferOwnership)
    }
}

/// Trusted facts about the acting principal in one tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subject {
    /// `principals.state = 'active'`.
    pub principal_active: bool,
    pub tenant_state: ResourceState,
    /// The tenant role, only when the membership is `active`.
    pub tenant_role: Option<Role>,
    /// Explicit sensitive grants (`member_permissions`).
    pub permissions: BTreeSet<SensitivePermission>,
}

/// The resource acted on, with its lifecycle state and the principal's effective role on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Tenant,
    /// `member_role` is the principal's active team-member role, if any.
    Team {
        state: ResourceState,
        member_role: Option<Role>,
    },
    /// `state` is the worst state along the resource's chain (document, project, workspace);
    /// `role` is the strongest effective role, already combined from every grant source.
    Workspace {
        state: ResourceState,
        role: Option<Role>,
    },
    Project {
        state: ResourceState,
        role: Option<Role>,
    },
    Document {
        state: ResourceState,
        role: Option<Role>,
    },
}

impl Resource {
    #[must_use]
    pub const fn scope(self) -> Scope {
        match self {
            Self::Tenant => Scope::Tenant,
            Self::Team { .. } => Scope::Team,
            Self::Workspace { .. } => Scope::Workspace,
            Self::Project { .. } => Scope::Project,
            Self::Document { .. } => Scope::Document,
        }
    }
}

/// Why a decision came out the way it did. Stable; safe to log and audit, never shown verbatim
/// to clients (map denials through the disclosure policy, S4-016).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Reason {
    Allowed,
    WrongResource,
    PrincipalInactive,
    TenantInactive,
    NotAMember,
    ResourceInactive,
    ResourceArchived,
    ResourceDeleting,
    NoRole,
    InsufficientRole,
    MissingPermission,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub allow: bool,
    pub reason: Reason,
}

impl Decision {
    const fn deny(reason: Reason) -> Self {
        Self { allow: false, reason }
    }

    const ALLOW: Self = Self { allow: true, reason: Reason::Allowed };
}

/// Resource-scope rank: tenant owners and admins act as workspace admins; `team_manager` ranks
/// nowhere (it never grants content access).
const fn resource_rank(role: Role) -> u8 {
    match role {
        Role::TenantOwner | Role::TenantAdmin | Role::WorkspaceAdmin => 4,
        Role::Editor => 3,
        Role::Commenter => 2,
        Role::Viewer => 1,
        Role::TeamManager => 0,
    }
}

/// The role→action matrix, independent of resource state and sensitive grants.
#[must_use]
pub const fn role_allows(role: Role, action: Action) -> bool {
    use Action as A;
    match action.scope() {
        Scope::Tenant => {
            let owner = matches!(role, Role::TenantOwner);
            let admin = owner || matches!(role, Role::TenantAdmin);
            let member = admin || matches!(role, Role::Editor | Role::Commenter | Role::Viewer);
            match action {
                A::TenantView | A::MemberView => member,
                A::TenantUpdatePolicy
                | A::TenantViewAudit
                | A::TenantExportAudit
                | A::MemberManage
                | A::ProviderKeysManage
                | A::SupportAccessGrant
                | A::ServiceAccountManage
                | A::TeamCreate
                | A::WorkspaceCreate => admin,
                A::TenantExport | A::TenantDelete | A::MemberManageOwners | A::MemberManagePermissions | A::BillingManage => owner,
                A::TenantTransferOwnership => false,
                _ => false,
            }
        }
        Scope::Team => {
            let manager = matches!(role, Role::TeamManager | Role::TenantOwner | Role::TenantAdmin);
            let member = manager || matches!(role, Role::Editor | Role::Commenter | Role::Viewer);
            match action {
                A::TeamView => member,
                A::TeamManage => manager,
                _ => false,
            }
        }
        Scope::Workspace | Scope::Project | Scope::Document => {
            let rank = resource_rank(role);
            let needed = match action {
                A::WorkspaceView | A::ProjectView | A::DocumentView | A::DocumentJoinRealtime | A::DocumentPresence | A::DocumentExport => 1,
                A::DocumentComment => 2,
                A::ProjectCreate
                | A::ProjectUpdate
                | A::DocumentCreate
                | A::DocumentEdit
                | A::DocumentCreateVersion
                | A::DocumentShare
                | A::DocumentRename => 3,
                A::WorkspaceUpdate
                | A::WorkspaceManageMembers
                | A::WorkspaceArchive
                | A::WorkspaceRestore
                | A::WorkspaceDelete
                | A::ProjectArchive
                | A::ProjectRestore
                | A::ProjectDelete
                | A::DocumentArchive
                | A::DocumentRestore
                | A::DocumentDelete => 4,
                _ => u8::MAX,
            };
            rank >= needed && rank > 0
        }
    }
}

/// Decide whether `subject` may perform `action` on `resource`. Deny by default.
#[must_use]
pub fn decide(subject: &Subject, resource: &Resource, action: Action) -> Decision {
    if resource.scope() != action.scope() {
        return Decision::deny(Reason::WrongResource);
    }
    if action.unsupported() {
        return Decision::deny(Reason::Unsupported);
    }
    if !subject.principal_active {
        return Decision::deny(Reason::PrincipalInactive);
    }
    if subject.tenant_state != ResourceState::Active {
        return Decision::deny(Reason::TenantInactive);
    }
    let Some(tenant_role) = subject.tenant_role else {
        return Decision::deny(Reason::NotAMember);
    };
    let (state, role) = match *resource {
        Resource::Tenant => (ResourceState::Active, Some(tenant_role)),
        Resource::Team { state, member_role } => {
            // Tenant owners and admins administer every team in their tenant.
            let role = if matches!(tenant_role, Role::TenantOwner | Role::TenantAdmin) { Some(tenant_role) } else { member_role };
            (state, role)
        }
        Resource::Workspace { state, role } | Resource::Project { state, role } | Resource::Document { state, role } => (state, role),
    };
    match state {
        ResourceState::Active => {}
        ResourceState::Archived if action.allowed_when_archived() => {}
        ResourceState::Archived => return Decision::deny(Reason::ResourceArchived),
        ResourceState::Deleting if action.allowed_when_deleting() => {}
        ResourceState::Deleting => return Decision::deny(Reason::ResourceDeleting),
        ResourceState::Pending | ResourceState::Suspended | ResourceState::Deleted => return Decision::deny(Reason::ResourceInactive),
    }
    let Some(role) = role else {
        return Decision::deny(Reason::NoRole);
    };
    if !role_allows(role, action) {
        return Decision::deny(Reason::InsufficientRole);
    }
    if let Some(permission) = action.sensitive_permission()
        && !subject.permissions.contains(&permission)
    {
        return Decision::deny(Reason::MissingPermission);
    }
    Decision::ALLOW
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 7] =
        [Role::TenantOwner, Role::TenantAdmin, Role::TeamManager, Role::WorkspaceAdmin, Role::Editor, Role::Commenter, Role::Viewer];

    fn letter(role: Role) -> char {
        match role {
            Role::TenantOwner => 'O',
            Role::TenantAdmin => 'A',
            Role::TeamManager => 'M',
            Role::WorkspaceAdmin => 'W',
            Role::Editor => 'E',
            Role::Commenter => 'C',
            Role::Viewer => 'V',
        }
    }

    /// The expected matrix, written out independently of `role_allows`. Letters are the roles
    /// that may perform the action: O tenant_owner, A tenant_admin, M team_manager, W
    /// workspace_admin, E editor, C commenter, V viewer. Every action must appear exactly once.
    const EXPECTED: &[(&str, &str)] = &[
        ("tenant.view", "OAECV"),
        ("tenant.update_policy", "OA"),
        ("tenant.view_audit", "OA"),
        ("tenant.export_audit", "OA"),
        ("tenant.export", "O"),
        ("tenant.delete", "O"),
        ("tenant.transfer_ownership", ""),
        ("member.view", "OAECV"),
        ("member.manage", "OA"),
        ("member.manage_owners", "O"),
        ("member.manage_permissions", "O"),
        ("billing.manage", "O"),
        ("provider_keys.manage", "OA"),
        ("support.grant", "OA"),
        ("service_account.manage", "OA"),
        ("team.create", "OA"),
        ("workspace.create", "OA"),
        ("team.view", "OAMECV"),
        ("team.manage", "OAM"),
        ("workspace.view", "OAWECV"),
        ("workspace.update", "OAW"),
        ("workspace.manage_members", "OAW"),
        ("workspace.archive", "OAW"),
        ("workspace.restore", "OAW"),
        ("workspace.delete", "OAW"),
        ("project.create", "OAWE"),
        ("project.view", "OAWECV"),
        ("project.update", "OAWE"),
        ("project.archive", "OAW"),
        ("project.restore", "OAW"),
        ("project.delete", "OAW"),
        ("document.create", "OAWE"),
        ("document.view", "OAWECV"),
        ("document.join_realtime", "OAWECV"),
        ("document.presence", "OAWECV"),
        ("document.comment", "OAWEC"),
        ("document.edit", "OAWE"),
        ("document.create_version", "OAWE"),
        ("document.export", "OAWECV"),
        ("document.share", "OAWE"),
        ("document.rename", "OAWE"),
        ("document.archive", "OAW"),
        ("document.restore", "OAW"),
        ("document.delete", "OAW"),
    ];

    fn active_subject(role: Role) -> Subject {
        Subject {
            principal_active: true,
            tenant_state: ResourceState::Active,
            tenant_role: Some(role),
            permissions: SensitivePermission::ALL.into_iter().collect(),
        }
    }

    fn resource_for(scope: Scope, state: ResourceState, role: Option<Role>) -> Resource {
        match scope {
            Scope::Tenant => Resource::Tenant,
            Scope::Team => Resource::Team { state, member_role: role },
            Scope::Workspace => Resource::Workspace { state, role },
            Scope::Project => Resource::Project { state, role },
            Scope::Document => Resource::Document { state, role },
        }
    }

    #[test]
    fn every_role_action_pair_matches_the_expected_matrix() {
        assert_eq!(EXPECTED.len(), Action::ALL.len(), "every action needs an expected row");
        let mut seen = BTreeSet::new();
        for &action in Action::ALL {
            let row = EXPECTED.iter().find(|(id, _)| *id == action.id());
            let Some((_, allowed)) = row else {
                panic!("no expected row for {}", action.id());
            };
            assert!(seen.insert(action.id()), "duplicate id {}", action.id());
            for role in ROLES {
                let expected = allowed.contains(letter(role));
                assert_eq!(role_allows(role, action), expected, "{} for {role:?}", action.id());
            }
        }
    }

    #[test]
    fn decide_follows_the_matrix_for_active_members_with_every_permission() {
        for &action in Action::ALL {
            for role in ROLES {
                // Tenant actions are decided by the tenant role; other scopes by the resource role
                // (the tenant role is a plain member so it does not lend team administration).
                let (subject, resource) = if action.scope() == Scope::Tenant {
                    (active_subject(role), Resource::Tenant)
                } else {
                    (active_subject(Role::Editor), resource_for(action.scope(), ResourceState::Active, Some(role)))
                };
                let decision = decide(&subject, &resource, action);
                let expected = role_allows(role, action);
                assert_eq!(decision.allow, expected, "{} for {role:?}: {decision:?}", action.id());
                if !expected {
                    let reason = if action == Action::TenantTransferOwnership { Reason::Unsupported } else { Reason::InsufficientRole };
                    assert_eq!(decision.reason, reason, "{}", action.id());
                }
            }
        }
    }

    #[test]
    fn sensitive_actions_need_an_explicit_grant_even_for_owners() {
        for &action in Action::ALL {
            let Some(permission) = action.sensitive_permission() else {
                continue;
            };
            let mut owner = active_subject(Role::TenantOwner);
            owner.permissions.clear();
            assert_eq!(decide(&owner, &Resource::Tenant, action), Decision::deny(Reason::MissingPermission), "{}", action.id());
            // Other permissions do not substitute.
            owner.permissions = SensitivePermission::ALL.into_iter().filter(|p| *p != permission).collect();
            assert_eq!(decide(&owner, &Resource::Tenant, action).reason, Reason::MissingPermission);
            owner.permissions.insert(permission);
            assert!(decide(&owner, &Resource::Tenant, action).allow, "{}", action.id());
            // A grant never lifts an insufficient role.
            let mut viewer = active_subject(Role::Viewer);
            viewer.permissions.insert(permission);
            assert_eq!(decide(&viewer, &Resource::Tenant, action).reason, Reason::InsufficientRole);
        }
        // Granting permissions needs no sensitive permission itself (bootstrap).
        let mut owner = active_subject(Role::TenantOwner);
        owner.permissions.clear();
        assert!(decide(&owner, &Resource::Tenant, Action::MemberManagePermissions).allow);
    }

    #[test]
    fn inactive_tenants_principals_and_non_members_are_denied_everything() {
        for &action in Action::ALL {
            let resource = resource_for(action.scope(), ResourceState::Active, Some(Role::TenantOwner));
            for state in [ResourceState::Pending, ResourceState::Suspended, ResourceState::Archived, ResourceState::Deleting, ResourceState::Deleted]
            {
                let mut subject = active_subject(Role::TenantOwner);
                subject.tenant_state = state;
                assert!(!decide(&subject, &resource, action).allow, "{} in {state:?}", action.id());
            }
            let mut subject = active_subject(Role::TenantOwner);
            subject.principal_active = false;
            assert!(!decide(&subject, &resource, action).allow);
            let mut subject = active_subject(Role::TenantOwner);
            subject.tenant_role = None;
            assert!(!decide(&subject, &resource, action).allow);
        }
    }

    #[test]
    fn resource_state_limits_actions() {
        let subject = active_subject(Role::TenantOwner);
        for &action in Action::ALL {
            if matches!(action.scope(), Scope::Tenant | Scope::Team) {
                continue;
            }
            let at = |state| decide(&subject, &resource_for(action.scope(), state, Some(Role::WorkspaceAdmin)), action);
            assert!(at(ResourceState::Active).allow, "{}", action.id());
            assert_eq!(at(ResourceState::Archived).allow, action.allowed_when_archived(), "{}", action.id());
            assert_eq!(at(ResourceState::Deleting).allow, action.allowed_when_deleting(), "{}", action.id());
            for state in [ResourceState::Pending, ResourceState::Suspended, ResourceState::Deleted] {
                assert_eq!(at(state).reason, Reason::ResourceInactive);
            }
        }
        let viewer = active_subject(Role::Viewer);
        let archived = Resource::Document { state: ResourceState::Archived, role: Some(Role::Editor) };
        assert!(decide(&viewer, &archived, Action::DocumentView).allow);
        assert_eq!(decide(&viewer, &archived, Action::DocumentEdit).reason, Reason::ResourceArchived);
    }

    #[test]
    fn wrong_resource_kind_and_missing_role_are_denied() {
        let subject = active_subject(Role::TenantOwner);
        let document = Resource::Document { state: ResourceState::Active, role: Some(Role::Editor) };
        assert_eq!(decide(&subject, &document, Action::WorkspaceView).reason, Reason::WrongResource);
        assert_eq!(decide(&subject, &Resource::Tenant, Action::DocumentView).reason, Reason::WrongResource);
        let none = Resource::Document { state: ResourceState::Active, role: None };
        assert_eq!(decide(&subject, &none, Action::DocumentView).reason, Reason::NoRole);
        // Team managers get nothing on content.
        let managed = Resource::Document { state: ResourceState::Active, role: Some(Role::TeamManager) };
        assert_eq!(decide(&subject, &managed, Action::DocumentView).reason, Reason::InsufficientRole);
    }

    #[test]
    fn tenant_admins_administer_every_team_but_members_need_a_team_role() {
        let team = Resource::Team { state: ResourceState::Active, member_role: None };
        assert!(decide(&active_subject(Role::TenantAdmin), &team, Action::TeamManage).allow);
        assert_eq!(decide(&active_subject(Role::Editor), &team, Action::TeamView).reason, Reason::NoRole);
        let viewer_team = Resource::Team { state: ResourceState::Active, member_role: Some(Role::Viewer) };
        assert!(decide(&active_subject(Role::Editor), &viewer_team, Action::TeamView).allow);
        assert!(!decide(&active_subject(Role::Editor), &viewer_team, Action::TeamManage).allow);
    }

    #[test]
    fn action_ids_are_unique_and_permissions_round_trip() {
        let ids: BTreeSet<_> = Action::ALL.iter().map(|action| action.id()).collect();
        assert_eq!(ids.len(), Action::ALL.len());
        for permission in SensitivePermission::ALL {
            assert_eq!(SensitivePermission::parse(permission.as_str()), Some(permission));
        }
        assert_eq!(SensitivePermission::parse("BILLING"), None);
    }
}
