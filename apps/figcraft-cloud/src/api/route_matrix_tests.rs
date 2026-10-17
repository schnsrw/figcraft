//! S4-021 and S4-022 at the API level: the full route x caller permission matrix and the
//! cross-tenant suite, through the production router ([`api_router`]) and the production
//! [`CredentialAuthenticator`](super::authenticator::CredentialAuthenticator) (fixture prefix e2).
//!
//! * [`ROWS`] lists every route `api_router` registers, with the request each cell sends and the
//!   expected outcome for every caller in [`WHO`]. The expectations are written from the
//!   product rules (module docs of `authz`, `tenants_api`, `teams_api`, `documents_api`,
//!   `authenticator` and the disclosure policy in `error`), not derived from the handlers.
//! * [`every_route_is_listed_and_nothing_else_is_registered`] (no database) proves the table
//!   matches the router: every listed method answers something other than a missing route, every
//!   other method on a listed path is `405`, and a sweep of plausible unlisted paths (object
//!   download and presign, exports, jobs, service accounts, audit, realtime tickets...) is `404`
//!   for every method. A route added to the router without a row fails this test.
//! * [`every_route_in_the_sources_has_a_row_and_every_row_is_in_the_sources`] (no database) reads
//!   every `.route(..)` registration in the route modules (and checks `api_router` merges nothing
//!   else), so the `(method, path)` set of [`ROWS`] equals what is registered, exactly.
//! * The relay action matrix (share editor/viewer links, member session tokens per role, revoked,
//!   other tenant, archived) is `tests/relay_action_matrix.rs`, through `CloudBackend::admit`.
//! * [`every_route_and_caller_matches_the_expected_matrix`] runs every row for every caller
//!   against the dev PostgreSQL (row-level security on), with real sessions
//!   (`sessions::create_session`), service-account bearer credentials (one per scope) and share
//!   sessions minted from real share links. Mutating cells get fresh resources each, so every cell
//!   is independent and reruns pass single-threaded.
//! * [`cross_tenant_resources_are_invisible_to_another_tenants_owner`] (S4-022) creates every
//!   resource kind in tenant A and shows that tenant B's owner gets the same `404` as for a random
//!   identifier on every read, update, delete and child route, that `/tenants/{A}/...` is a
//!   uniform `403` (the same as for a tenant that does not exist), that B's lists never include
//!   A's identifiers, that the collaboration-link (share-secret minting) route never crosses
//!   tenants, and that A's resources are unchanged afterwards.
//!
//! Database tests are skipped without `FIGCRAFT_TEST_DATABASE_URL_FILE`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::invitations::create_invitation;
use figcraft_cloud_storage::memberships::{
    MemberPermission, add_tenant_member, grant_member_permission, set_document_member, set_document_team_grant, set_workspace_member,
    suspend_tenant_member,
};
use figcraft_cloud_storage::metadata::{archive_document, create_document, create_project, create_workspace};
use figcraft_cloud_storage::resource_admin::{Transition, transition_project, transition_workspace};
use figcraft_cloud_storage::service_accounts::{Scope, create_service_account, issue_credential};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, exchange_share_token};
use figcraft_cloud_storage::teams::{self, OwnedResource, TeamGrant, TeamRole, WorkspaceGrantRole};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

use super::authenticator::{IdentityConfig, SHARE_COOKIE, TENANT_HEADER};
use super::context::ContextSettings;
use super::rate_limit::RateLimitConfig;
use super::{ApiConfig, AppState, api_router};
use crate::config::CloudRole;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

const ORIGIN: &str = "https://figcraft-e2.test";

// ---------------------------------------------------------------------------------------------
// Callers

/// Every kind of caller the matrix covers.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Who {
    Anonymous,
    /// Service accounts in tenant A, one per allowlisted scope.
    ServiceRead,
    ServiceWrite,
    ServiceExport,
    ServiceJobs,
    /// Tenant members of A with the same role on the workspace under test (`V`, `C`, `E`).
    Viewer,
    Commenter,
    Editor,
    /// A tenant `editor` who is `workspace_admin` of the workspace under test (`W`).
    WorkspaceAdmin,
    /// `A` and `O`: tenant roles that act as workspace admins everywhere.
    TenantAdmin,
    TenantOwner,
    /// A signed-in principal with no membership in A, naming A.
    NonMember,
    /// A suspended member of A (session still live).
    Suspended,
    /// Tenant B's owner acting in B, sending A's identifiers.
    OtherTenant,
    /// Share sessions from an editor and a viewer link on the document under test.
    ShareEditor,
    ShareViewer,
}

const WHO: [Who; 16] = [
    Who::Anonymous,
    Who::ServiceRead,
    Who::ServiceWrite,
    Who::ServiceExport,
    Who::ServiceJobs,
    Who::Viewer,
    Who::Commenter,
    Who::Editor,
    Who::WorkspaceAdmin,
    Who::TenantAdmin,
    Who::TenantOwner,
    Who::NonMember,
    Who::Suspended,
    Who::OtherTenant,
    Who::ShareEditor,
    Who::ShareViewer,
];

impl Who {
    /// The member letter used in [`Row::allow`].
    const fn letter(self) -> Option<char> {
        match self {
            Self::Viewer => Some('V'),
            Self::Commenter => Some('C'),
            Self::Editor => Some('E'),
            Self::WorkspaceAdmin => Some('W'),
            Self::TenantAdmin => Some('A'),
            Self::TenantOwner => Some('O'),
            _ => None,
        }
    }

    const fn service(self) -> bool {
        matches!(self, Self::ServiceRead | Self::ServiceWrite | Self::ServiceExport | Self::ServiceJobs)
    }
}

// ---------------------------------------------------------------------------------------------
// The expected table

/// What kind of route a row is, which decides the outcome for callers that are not members.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// `/auth/*`: fixed outcomes (no OIDC provider is configured here, multi-tenant mode).
    Auth(AuthRoute),
    /// `GET /tenants`: the caller's own tenants.
    TenantsList,
    /// `POST /tenants`: people in multi-tenant mode.
    TenantsCreate,
    /// `POST /invitations/accept` with an unknown token.
    Accept,
    /// A tenant-scope action in the caller's own tenant (`/workspaces`): denied members `403`.
    CallerTenant,
    /// Under `/tenants/{A}`: a member who may not act gets `deny`.
    TenantPath { deny: u16 },
    /// A workspace, project or document identified in the path: a member who may not act gets
    /// `deny` (every member here holds a role on it, so it is visible: `403`).
    Resource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthRoute {
    Login,
    Callback,
    Logout,
    BackchannelLogout,
    Bootstrap,
    Exchange,
    Session,
}

/// Request body a row sends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Send {
    Nothing,
    Token,
    Name,
    NewTenant,
    AddTenantMember,
    MemberUpdate,
    NewTeam,
    AddTeamMember,
    RoleViewer,
    RoleEditor,
    ShareLink,
    Invitation,
}

/// State a cell needs before its request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Prep {
    None,
    ArchiveWorkspace,
    ArchiveProject,
    ArchiveDocument,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    method: &'static str,
    /// The route as registered (relative to `/api/v1`).
    route: &'static str,
    /// The concrete path: `{t}` tenant, `{w}` workspace, `{p}` project, `{d}` document, `{team}`,
    /// `{m}` a tenant admin who is in the team, `{m2}` a tenant viewer, `{link}` a share link,
    /// `{inv}` an invitation.
    path: &'static str,
    send: Send,
    prep: Prep,
    kind: Kind,
    /// Members (`V C E W A O`) allowed to perform it (2xx).
    allow: &'static str,
}

const fn row(method: &'static str, route: &'static str, path: &'static str, send: Send, kind: Kind, allow: &'static str) -> Row {
    Row { method, route, path, send, prep: Prep::None, kind, allow }
}

const fn prepped(row: Row, prep: Prep) -> Row {
    Row { prep, ..row }
}

const ALL: &str = "VCEWAO";
const ADMINS: &str = "AO";
const WS_ADMINS: &str = "WAO";
const EDITORS: &str = "EWAO";
const OWNER: &str = "O";
const AUTH: fn(AuthRoute) -> Kind = Kind::Auth;
const TP403: Kind = Kind::TenantPath { deny: 403 };
const TP404: Kind = Kind::TenantPath { deny: 404 };
const RES: Kind = Kind::Resource;

/// Every route `api_router` registers. Team routes: only team members see a team, and tenant
/// owners and admins administer every team, so other members get `404`; withdrawing a team's
/// workspace grant is the workspace admin's call. Sensitive-permission management is owner-only.
#[rustfmt::skip]
const ROWS: &[Row] = &[
    // /auth (rate limited)
    row("GET", "/auth/login", "/auth/login", Send::Nothing, Kind::Auth(AuthRoute::Login), ""),
    row("GET", "/auth/callback", "/auth/callback", Send::Nothing, Kind::Auth(AuthRoute::Callback), ""),
    row("POST", "/auth/logout", "/auth/logout", Send::Nothing, Kind::Auth(AuthRoute::Logout), ""),
    row("POST", "/auth/backchannel-logout", "/auth/backchannel-logout", Send::Nothing, Kind::Auth(AuthRoute::BackchannelLogout), ""),
    row("POST", "/auth/bootstrap/complete", "/auth/bootstrap/complete", Send::Token, Kind::Auth(AuthRoute::Bootstrap), ""),
    row("POST", "/auth/share/exchange", "/auth/share/exchange", Send::Token, Kind::Auth(AuthRoute::Exchange), ""),
    row("GET", "/auth/session", "/auth/session", Send::Nothing, Kind::Auth(AuthRoute::Session), ""),
    // tenants and members
    row("GET", "/tenants", "/tenants", Send::Nothing, Kind::TenantsList, ALL),
    row("POST", "/tenants", "/tenants", Send::NewTenant, Kind::TenantsCreate, ALL),
    row("GET", "/tenants/{tenant}", "/tenants/{t}", Send::Nothing, TP403, ALL),
    row("PATCH", "/tenants/{tenant}", "/tenants/{t}", Send::Name, TP403, ADMINS),
    row("GET", "/tenants/{tenant}/members", "/tenants/{t}/members", Send::Nothing, TP403, ALL),
    row("POST", "/tenants/{tenant}/members", "/tenants/{t}/members", Send::AddTenantMember, TP403, ADMINS),
    row("PATCH", "/tenants/{tenant}/members/{principal}", "/tenants/{t}/members/{m}", Send::MemberUpdate, TP403, ADMINS),
    row("DELETE", "/tenants/{tenant}/members/{principal}", "/tenants/{t}/members/{m}", Send::Nothing, TP403, ADMINS),
    row("GET", "/tenants/{tenant}/members/{principal}/permissions", "/tenants/{t}/members/{m}/permissions", Send::Nothing, TP403, OWNER),
    row("PUT", "/tenants/{tenant}/members/{principal}/permissions/{permission}", "/tenants/{t}/members/{m}/permissions/provider_keys", Send::Nothing, TP403, OWNER),
    row("DELETE", "/tenants/{tenant}/members/{principal}/permissions/{permission}", "/tenants/{t}/members/{m}/permissions/support", Send::Nothing, TP403, OWNER),
    // teams and invitations
    row("GET", "/tenants/{tenant}/teams", "/tenants/{t}/teams", Send::Nothing, TP403, ALL),
    row("POST", "/tenants/{tenant}/teams", "/tenants/{t}/teams", Send::NewTeam, TP403, ADMINS),
    row("GET", "/tenants/{tenant}/teams/{team}", "/tenants/{t}/teams/{team}", Send::Nothing, TP404, ADMINS),
    row("PATCH", "/tenants/{tenant}/teams/{team}", "/tenants/{t}/teams/{team}", Send::Name, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}", "/tenants/{t}/teams/{team}", Send::Nothing, TP404, ADMINS),
    row("GET", "/tenants/{tenant}/teams/{team}/members", "/tenants/{t}/teams/{team}/members", Send::Nothing, TP404, ADMINS),
    row("POST", "/tenants/{tenant}/teams/{team}/members", "/tenants/{t}/teams/{team}/members", Send::AddTeamMember, TP404, ADMINS),
    row("PATCH", "/tenants/{tenant}/teams/{team}/members/{principal}", "/tenants/{t}/teams/{team}/members/{m}", Send::RoleViewer, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}/members/{principal}", "/tenants/{t}/teams/{team}/members/{m}", Send::Nothing, TP404, ADMINS),
    row("PUT", "/tenants/{tenant}/teams/{team}/grants/workspaces/{workspace}", "/tenants/{t}/teams/{team}/grants/workspaces/{w}", Send::RoleEditor, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}/grants/workspaces/{workspace}", "/tenants/{t}/teams/{team}/grants/workspaces/{w}", Send::Nothing, TP403, WS_ADMINS),
    row("PUT", "/tenants/{tenant}/teams/{team}/owned/workspaces/{workspace}", "/tenants/{t}/teams/{team}/owned/workspaces/{w}", Send::Nothing, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}/owned/workspaces/{workspace}", "/tenants/{t}/teams/{team}/owned/workspaces/{w}", Send::Nothing, TP404, ADMINS),
    row("PUT", "/tenants/{tenant}/teams/{team}/owned/projects/{project}", "/tenants/{t}/teams/{team}/owned/projects/{p}", Send::Nothing, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}/owned/projects/{project}", "/tenants/{t}/teams/{team}/owned/projects/{p}", Send::Nothing, TP404, ADMINS),
    row("GET", "/tenants/{tenant}/invitations", "/tenants/{t}/invitations", Send::Nothing, TP403, ADMINS),
    row("GET", "/tenants/{tenant}/teams/{team}/invitations", "/tenants/{t}/teams/{team}/invitations", Send::Nothing, TP404, ADMINS),
    row("POST", "/tenants/{tenant}/teams/{team}/invitations", "/tenants/{t}/teams/{team}/invitations", Send::Invitation, TP404, ADMINS),
    row("DELETE", "/tenants/{tenant}/teams/{team}/invitations/{invitation}", "/tenants/{t}/teams/{team}/invitations/{inv}", Send::Nothing, TP404, ADMINS),
    row("POST", "/invitations/accept", "/invitations/accept", Send::Token, Kind::Accept, ALL),
    // workspaces
    row("GET", "/workspaces", "/workspaces", Send::Nothing, Kind::CallerTenant, ALL),
    row("POST", "/workspaces", "/workspaces", Send::Name, Kind::CallerTenant, ADMINS),
    row("GET", "/workspaces/{workspace_id}", "/workspaces/{w}", Send::Nothing, RES, ALL),
    row("PATCH", "/workspaces/{workspace_id}", "/workspaces/{w}", Send::Name, RES, WS_ADMINS),
    row("DELETE", "/workspaces/{workspace_id}", "/workspaces/{w}", Send::Nothing, RES, WS_ADMINS),
    row("POST", "/workspaces/{workspace_id}/archive", "/workspaces/{w}/archive", Send::Nothing, RES, WS_ADMINS),
    prepped(row("POST", "/workspaces/{workspace_id}/restore", "/workspaces/{w}/restore", Send::Nothing, RES, WS_ADMINS), Prep::ArchiveWorkspace),
    row("GET", "/workspaces/{workspace_id}/members", "/workspaces/{w}/members", Send::Nothing, RES, ALL),
    row("PUT", "/workspaces/{workspace_id}/members/{principal_id}", "/workspaces/{w}/members/{m2}", Send::RoleEditor, RES, WS_ADMINS),
    row("DELETE", "/workspaces/{workspace_id}/members/{principal_id}", "/workspaces/{w}/members/{m2}", Send::Nothing, RES, WS_ADMINS),
    row("GET", "/workspaces/{workspace_id}/projects", "/workspaces/{w}/projects", Send::Nothing, RES, ALL),
    row("POST", "/workspaces/{workspace_id}/projects", "/workspaces/{w}/projects", Send::Name, RES, EDITORS),
    // projects
    row("GET", "/projects/{project_id}", "/projects/{p}", Send::Nothing, RES, ALL),
    row("PATCH", "/projects/{project_id}", "/projects/{p}", Send::Name, RES, EDITORS),
    row("DELETE", "/projects/{project_id}", "/projects/{p}", Send::Nothing, RES, WS_ADMINS),
    row("POST", "/projects/{project_id}/archive", "/projects/{p}/archive", Send::Nothing, RES, WS_ADMINS),
    prepped(row("POST", "/projects/{project_id}/restore", "/projects/{p}/restore", Send::Nothing, RES, WS_ADMINS), Prep::ArchiveProject),
    row("GET", "/projects/{project_id}/documents", "/projects/{p}/documents", Send::Nothing, RES, ALL),
    row("POST", "/projects/{project_id}/documents", "/projects/{p}/documents", Send::Name, RES, EDITORS),
    // documents
    row("GET", "/documents/{document_id}", "/documents/{d}", Send::Nothing, RES, ALL),
    row("PATCH", "/documents/{document_id}", "/documents/{d}", Send::Name, RES, EDITORS),
    row("DELETE", "/documents/{document_id}", "/documents/{d}", Send::Nothing, RES, WS_ADMINS),
    row("POST", "/documents/{document_id}/archive", "/documents/{d}/archive", Send::Nothing, RES, WS_ADMINS),
    prepped(row("POST", "/documents/{document_id}/restore", "/documents/{d}/restore", Send::Nothing, RES, WS_ADMINS), Prep::ArchiveDocument),
    row("GET", "/documents/{document_id}/members", "/documents/{d}/members", Send::Nothing, RES, ALL),
    row("PUT", "/documents/{document_id}/members/{principal_id}", "/documents/{d}/members/{m2}", Send::RoleEditor, RES, EDITORS),
    row("DELETE", "/documents/{document_id}/members/{principal_id}", "/documents/{d}/members/{m2}", Send::Nothing, RES, EDITORS),
    row("GET", "/documents/{document_id}/team-grants", "/documents/{d}/team-grants", Send::Nothing, RES, ALL),
    row("PUT", "/documents/{document_id}/team-grants/{team_id}", "/documents/{d}/team-grants/{team}", Send::RoleEditor, RES, EDITORS),
    row("DELETE", "/documents/{document_id}/team-grants/{team_id}", "/documents/{d}/team-grants/{team}", Send::Nothing, RES, EDITORS),
    row("GET", "/documents/{document_id}/share-links", "/documents/{d}/share-links", Send::Nothing, RES, EDITORS),
    row("POST", "/documents/{document_id}/share-links", "/documents/{d}/share-links", Send::ShareLink, RES, EDITORS),
    row("DELETE", "/documents/{document_id}/share-links/{link_id}", "/documents/{d}/share-links/{link}", Send::Nothing, RES, EDITORS),
    row("GET", "/documents/{document_id}/collab-link", "/documents/{d}/collab-link", Send::Nothing, RES, ALL),
    row("POST", "/documents/{document_id}/realtime-ticket", "/documents/{d}/realtime-ticket", Send::Nothing, RES, ALL),
];

/// The expected outcome: `None` is any 2xx.
type Expected = Option<u16>;

/// The outcome for `who` on `row`, from the rules alone.
fn expected(row: &Row, who: Who) -> Expected {
    if let Kind::Auth(route) = row.kind {
        return match route {
            // No OIDC provider configured: login and callback are unavailable for everyone.
            // Back-channel logout too: it needs the provider's keys (never a caller credential).
            AuthRoute::Login | AuthRoute::Callback | AuthRoute::BackchannelLogout => Some(503),
            // Idempotent, same-origin (every request here carries our Origin).
            AuthRoute::Logout => Some(204),
            // Bootstrap exists only in single-tenant deployments.
            AuthRoute::Bootstrap => Some(404),
            // An unknown share token is refused like a bad credential.
            AuthRoute::Exchange => Some(401),
            AuthRoute::Session => (who == Who::Anonymous).then_some(401),
        };
    }
    match who {
        Who::Anonymous => Some(401),
        // A live session or link that may not act in tenant A: the authenticator's uniform 403
        // (non-members and suspended members; share sessions off their own document, and the
        // management API refuses share sessions on it too).
        // Archiving a document ends its share sessions: the cookie no longer authenticates.
        Who::ShareEditor | Who::ShareViewer if row.prep == Prep::ArchiveDocument => Some(401),
        Who::NonMember | Who::Suspended | Who::ShareEditor | Who::ShareViewer => Some(403),
        // Service principals are not tenant members: no tenant role, no resource role, whatever
        // the scope. Their own tenant list is readable; people-only routes are 403.
        // Realtime tickets are for browser sessions only: every other credential is refused before
        // any lookup, so the answer does not depend on the document.
        _ if who.service() && row.path.ends_with("/realtime-ticket") => Some(403),
        _ if who.service() => match row.kind {
            Kind::TenantsList => None,
            Kind::TenantsCreate | Kind::Accept | Kind::CallerTenant => Some(403),
            _ => Some(404),
        },
        // B's owner acting in B: their own lists and creations work in B; A's tenant path is the
        // authenticator's 403 (header B, path A); A's resources do not exist in B.
        Who::OtherTenant => match row.kind {
            Kind::TenantsList | Kind::TenantsCreate => None,
            Kind::CallerTenant => {
                if row.allow.contains('O') {
                    None
                } else {
                    Some(403)
                }
            }
            Kind::Accept => Some(404),
            Kind::TenantPath { .. } => Some(403),
            _ => Some(404),
        },
        _ => {
            let letter = who.letter().unwrap_or('?');
            let allowed = row.allow.contains(letter);
            match row.kind {
                // An unknown invitation token is 404 for anyone allowed to try.
                Kind::Accept => Some(404),
                _ if allowed => None,
                Kind::TenantPath { deny } => Some(deny),
                _ => Some(403),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The app and requests

fn objects() -> figcraft_cloud_storage::objects::ObjectStore {
    figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .unwrap()
}

fn identity() -> Arc<IdentityConfig> {
    Arc::new(IdentityConfig::new(DeploymentMode::MultiTenant, None, Some(ORIGIN), Some(b"e2-route-matrix-signing-key-0123")).unwrap())
}

/// The production stack: `api_router` with the credential authenticator, behind the request
/// context middleware. The auth rate limit is raised so the matrix is not throttled.
fn app(pool: PgPool, identity: Arc<IdentityConfig>) -> Router {
    let config = ApiConfig {
        deployment: DeploymentMode::MultiTenant,
        auth_rate_limit: RateLimitConfig { burst: 1_000_000, refill_every: Duration::from_millis(1), max_keys: 1_000 },
        ..ApiConfig::default()
    };
    let ticket_key = figcraft_cloud_storage::realtime_tickets::TicketKey::derive(b"e2-route-matrix-realtime-ticket-key").expect("test ticket key");
    let state = AppState::new(pool, objects(), config).with_identity(identity).with_realtime_tickets(Arc::new(ticket_key));
    crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 256, Duration::from_secs(30), 1 << 20))
}

async fn send(app: &Router, method: &str, path: &str, headers: &[(String, String)], body: Option<&Value>) -> (u16, Value) {
    let peer: SocketAddr = "192.0.2.80:4000".parse().unwrap();
    let mut request =
        Request::builder().method(Method::from_bytes(method.as_bytes()).unwrap()).uri(format!("/api/v1{path}")).extension(ConnectInfo(peer));
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = match body {
        Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let mut reply = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    // The request ID differs per request; drop it so replies can be compared for equality.
    if let Some(error) = reply.get_mut("error").and_then(Value::as_object_mut) {
        error.remove("request_id");
    }
    (status, reply)
}

fn matches(expected: Expected, status: u16) -> bool {
    match expected {
        None => (200..300).contains(&status),
        Some(code) => code == status,
    }
}

// ---------------------------------------------------------------------------------------------
// S4-021: the table is the router

/// Plausible routes that must not exist (a new one has to be added to [`ROWS`] first).
const UNLISTED: &[&str] = &[
    "/auth",
    "/auth/token",
    "/auth/refresh",
    "/auth/share",
    "/invitations",
    "/documents",
    "/projects",
    "/documents/{d}/download",
    "/documents/{d}/presign",
    "/documents/{d}/content",
    "/documents/{d}/export",
    "/documents/{d}/exports",
    "/documents/{d}/versions",
    "/documents/{d}/comments",
    "/documents/{d}/members/{m2}/role",
    "/documents/{d}/share-links/{link}/secret",
    "/objects/{d}",
    "/assets/{d}",
    "/downloads/{d}",
    "/uploads",
    "/exports",
    "/jobs",
    "/realtime/tickets",
    "/service-accounts",
    "/tenants/{t}/service-accounts",
    "/tenants/{t}/audit",
    "/tenants/{t}/support-access",
    "/tenants/{t}/teams/{team}/grants",
    "/tenants/{t}/teams/{team}/grants/projects/{p}",
    "/tenants/{t}/teams/{team}/owned/libraries/{d}",
    "/tenants/{t}/members/{m}/sessions",
    "/workspaces/{w}/team-grants",
    "/projects/{p}/members",
    "/admin",
];

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Substitute placeholders with syntactically valid identifiers (no fixtures).
fn placeholder_path(path: &str) -> String {
    let id = |n: u8| format!("e2000000-0000-4000-8000-0000000000{n:02x}");
    path.replace("{t}", &id(1))
        .replace("{w}", &id(2))
        .replace("{p}", &id(3))
        .replace("{d}", &id(4))
        .replace("{team}", &id(5))
        .replace("{m2}", &id(7))
        .replace("{m}", &id(6))
        .replace("{link}", &id(8))
        .replace("{inv}", &id(9))
}

#[test]
fn the_table_has_one_row_per_route_and_covers_the_documents_module() {
    let mut seen = BTreeSet::new();
    for row in ROWS {
        assert!(seen.insert((row.method, row.route)), "duplicate row {} {}", row.method, row.route);
        assert!(METHODS.contains(&row.method), "{} {}", row.method, row.route);
    }
    // documents_api keeps its own ROUTES list; every entry must be a row here.
    for route in super::documents_api::ROUTES {
        assert!(seen.contains(&(route.method, route.path)), "documents_api route {} {} has no matrix row", route.method, route.path);
    }
    // Every member letter in the table is a real role letter.
    for row in ROWS {
        assert!(row.allow.chars().all(|letter| ALL.contains(letter)), "{} {}", row.method, row.route);
    }
}

/// The sources that register routes, with the mount prefix of each. `api_router` and
/// `tenants_api::routes` may only merge routers from this list (checked below), so a route added
/// in a new module fails until the module is listed here and its routes have rows.
const ROUTE_SOURCES: &[(&str, &str, &str)] = &[
    ("auth_routes", "/auth", include_str!("auth_routes.rs")),
    ("tenants_api", "", include_str!("tenants_api.rs")),
    ("teams_api", "", include_str!("teams_api.rs")),
    ("documents_api", "", include_str!("documents_api.rs")),
];

/// `(method, path)` for every `.route("...", get(..).post(..))` line in `source`. `teams_api`
/// writes its paths as `format!("{tenant}/teams/{{team}}")` with `tenant = "/tenants/{tenant}"`.
fn registered_routes(prefix: &str, source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in source.lines().filter(|line| line.trim_start().starts_with(".route(")) {
        let mut parts = line.splitn(3, '"');
        let (Some(_), Some(literal), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            panic!("cannot parse route line {line:?}: keep `.route(` calls on one line");
        };
        let path = literal.replace("{{", "\u{1}").replace("}}", "\u{2}");
        let path = path.strip_prefix("{tenant}").map_or(path.clone(), |tail| format!("/tenants/{{tenant}}{tail}"));
        let path = format!("{prefix}{}", path.replace('\u{1}', "{").replace('\u{2}', "}"));
        let mut methods = 0;
        for method in METHODS {
            let needle = format!("{}(", method.to_ascii_lowercase());
            for (at, _) in rest.match_indices(&needle) {
                let before = rest[..at].chars().next_back();
                if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    out.push((method.to_string(), path.clone()));
                    methods += 1;
                }
            }
        }
        assert!(methods > 0, "no method found on route line {line:?}");
    }
    out
}

#[test]
fn every_route_in_the_sources_has_a_row_and_every_row_is_in_the_sources() {
    let modules: BTreeSet<&str> = ROUTE_SOURCES.iter().map(|(name, _, _)| *name).collect();
    let mut merged = 0;
    for source in [include_str!("mod.rs"), include_str!("tenants_api.rs")] {
        for line in source.lines().filter(|line| !line.trim_start().starts_with("//")) {
            for (at, _) in line.match_indices("::routes()") {
                let name = line[..at].rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or_default();
                assert!(modules.contains(name), "a router from `{name}` is merged but not scanned by ROUTE_SOURCES");
                merged += 1;
            }
        }
    }
    // api_router merges auth_routes, tenants_api and documents_api; tenants_api merges teams_api.
    assert_eq!(merged, 4, "the set of merged routers changed: update ROUTE_SOURCES");
    let registered: BTreeSet<(String, String)> = ROUTE_SOURCES.iter().flat_map(|(_, prefix, source)| registered_routes(prefix, source)).collect();
    let listed: BTreeSet<(String, String)> = ROWS.iter().map(|row| (row.method.to_string(), row.route.to_string())).collect();
    let unlisted: Vec<_> = registered.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&registered).collect();
    assert!(unlisted.is_empty(), "registered routes without a matrix row: {unlisted:?}");
    assert!(stale.is_empty(), "matrix rows for routes that are not registered: {stale:?}");
    assert_eq!(registered.len(), ROWS.len());
}

#[tokio::test]
async fn every_route_is_listed_and_nothing_else_is_registered() {
    // No database is reachable: anonymous requests are refused before any lookup.
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap();
    let app = app(pool, identity());
    let origin = vec![("origin".to_string(), ORIGIN.to_string())];
    // Registered route -> (one concrete path for it, the methods listed for it).
    let mut by_route: BTreeMap<&str, (String, BTreeSet<&str>)> = BTreeMap::new();
    for row in ROWS {
        by_route.entry(row.route).or_insert_with(|| (placeholder_path(row.path), BTreeSet::new())).1.insert(row.method);
        let body = match row.send {
            Send::Nothing => None,
            _ => Some(json!({"token": "x"})),
        };
        let (status, reply) = send(&app, row.method, &placeholder_path(row.path), &origin, body.as_ref()).await;
        assert_ne!(status, 405, "{} {} is not registered for this method", row.method, row.route);
        assert!(matches(expected(row, Who::Anonymous), status), "anonymous {} {}: {status} {reply}", row.method, row.route);
    }
    // Every other method on a listed path is 405 (the path exists, the method does not), which
    // also proves the paths whose anonymous answer is a by-design 404 are registered.
    for (path, listed) in by_route.values() {
        for method in METHODS.into_iter().filter(|method| !listed.contains(method)) {
            let (status, _) = send(&app, method, path, &origin, None).await;
            assert_eq!(status, 405, "{method} {path} answers {status}: an unlisted route exists or the path is missing");
        }
    }
    for path in UNLISTED {
        let concrete = placeholder_path(path);
        for method in METHODS {
            let (status, reply) = send(&app, method, &concrete, &origin, None).await;
            assert_eq!((status, reply["error"]["code"].as_str()), (404, Some("not_found")), "{method} {path} is registered but not in ROWS");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Database fixtures

async fn pool() -> Option<PgPool> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; route matrix DB test skipped");
        return None;
    };
    let url = std::fs::read_to_string(file).ok()?;
    Some(sqlx::postgres::PgPoolOptions::new().max_connections(8).connect(url.trim_end()).await.unwrap())
}

fn e2() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xe2;
    Uuid::from_bytes(bytes)
}

fn e2_id<T: TryFrom<Uuid>>() -> T {
    T::try_from(e2()).ok().unwrap()
}

fn slug() -> String {
    format!("e2-{}", &e2().simple().to_string()[2..14])
}

fn err<E: std::fmt::Debug>(error: E) -> Box<dyn std::error::Error> {
    format!("{error:?}").into()
}

async fn principal(pool: &PgPool, name: &str) -> Fallible<PrincipalId> {
    let id: PrincipalId = e2_id();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(id.to_string())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn session(pool: &PgPool, principal: PrincipalId) -> Fallible<String> {
    Ok(create_session(pool, principal, SessionLimits::default()).await.map_err(err)?.token)
}

async fn tenant(pool: &PgPool, owner: PrincipalId, name: &str) -> Fallible<TenantId> {
    let tenant: TenantId = e2_id();
    let slug = slug();
    let mut t = begin_tenant_transaction(pool, tenant).await.map_err(err)?;
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name, plan_id: "team", home_region: "local" }, owner).await.map_err(err)?;
    t.commit().await?;
    Ok(tenant)
}

/// Tenant A with one member per role, B with its owner, and every credential.
struct World {
    pool: PgPool,
    identity: Arc<IdentityConfig>,
    app: Router,
    a: TenantId,
    b: TenantId,
    owner: PrincipalId,
    admin: PrincipalId,
    viewer: PrincipalId,
    commenter: PrincipalId,
    editor: PrincipalId,
    ws_admin: PrincipalId,
    b_owner: PrincipalId,
    sessions: BTreeMap<Who, String>,
    service: BTreeMap<Who, String>,
}

async fn world(pool: &PgPool) -> Fallible<World> {
    let owner = principal(pool, "e2 owner").await?;
    let admin = principal(pool, "e2 admin").await?;
    let viewer = principal(pool, "e2 viewer").await?;
    let commenter = principal(pool, "e2 commenter").await?;
    let editor = principal(pool, "e2 editor").await?;
    let ws_admin = principal(pool, "e2 workspace admin").await?;
    let suspended = principal(pool, "e2 suspended").await?;
    let outsider = principal(pool, "e2 outsider").await?;
    let b_owner = principal(pool, "e2 other owner").await?;
    let a = tenant(pool, owner, "e2 tenant A").await?;
    let b = tenant(pool, b_owner, "e2 tenant B").await?;
    let mut service = BTreeMap::new();
    {
        let mut t = begin_tenant_transaction(pool, a).await.map_err(err)?;
        for (who, role) in [
            (admin, Role::TenantAdmin),
            (viewer, Role::Viewer),
            (commenter, Role::Commenter),
            (editor, Role::Editor),
            (ws_admin, Role::Editor),
            (suspended, Role::Editor),
        ] {
            add_tenant_member(&mut t, who, role).await.map_err(err)?;
        }
        suspend_tenant_member(&mut t, suspended).await.map_err(err)?;
        for (who, scope) in [
            (Who::ServiceRead, Scope::DocumentsRead),
            (Who::ServiceWrite, Scope::DocumentsWrite),
            (Who::ServiceExport, Scope::ExportsCreate),
            (Who::ServiceJobs, Scope::JobsRead),
        ] {
            let account = create_service_account(&mut t, &format!("e2 {}", scope.as_str()), &[scope], owner).await.map_err(err)?;
            let issued = issue_credential(&mut t, account.id, 3_600, owner).await.map_err(err)?;
            service.insert(who, issued.secret.clone());
        }
        t.commit().await?;
    }
    let mut sessions = BTreeMap::new();
    for (who, principal) in [
        (Who::Viewer, viewer),
        (Who::Commenter, commenter),
        (Who::Editor, editor),
        (Who::WorkspaceAdmin, ws_admin),
        (Who::TenantAdmin, admin),
        (Who::TenantOwner, owner),
        (Who::NonMember, outsider),
        (Who::Suspended, suspended),
        (Who::OtherTenant, b_owner),
    ] {
        sessions.insert(who, session(pool, principal).await?);
    }
    let identity = identity();
    let app = app(pool.clone(), Arc::clone(&identity));
    Ok(World { pool: pool.clone(), identity, app, a, b, owner, admin, viewer, commenter, editor, ws_admin, b_owner, sessions, service })
}

/// Fresh resources in A for one cell (or one shared set for the read-only rows).
struct Fixture {
    workspace: WorkspaceId,
    project: ProjectId,
    document: CloudDocumentId,
    team: Uuid,
    /// A tenant admin of A, an editor of `team`, holding the `support` permission.
    member: PrincipalId,
    /// A tenant viewer of A, viewer of the workspace and the document.
    member2: PrincipalId,
    /// A principal that is not yet a member of A.
    newcomer: PrincipalId,
    link: Uuid,
    invitation: Uuid,
    share_editor: String,
    share_viewer: String,
}

async fn fixture(world: &World, prep: Prep) -> Fallible<Fixture> {
    let pool = &world.pool;
    let member = principal(pool, "e2 team member").await?;
    let member2 = principal(pool, "e2 second member").await?;
    let newcomer = principal(pool, "e2 newcomer").await?;
    let (workspace, project, document): (WorkspaceId, ProjectId, CloudDocumentId) = (e2_id(), e2_id(), e2_id());
    let team = e2();
    let mut t = begin_tenant_transaction(pool, world.a).await.map_err(err)?;
    add_tenant_member(&mut t, member, Role::TenantAdmin).await.map_err(err)?;
    add_tenant_member(&mut t, member2, Role::Viewer).await.map_err(err)?;
    grant_member_permission(&mut t, member, MemberPermission::Support, world.owner).await.map_err(err)?;
    create_workspace(&mut t, workspace, "e2 workspace", None).await.map_err(err)?;
    create_project(&mut t, project, workspace, "e2 project", None).await.map_err(err)?;
    create_document(&mut t, document, workspace, project, "e2 document").await.map_err(err)?;
    for (who, role) in [
        (world.viewer, Role::Viewer),
        (world.commenter, Role::Commenter),
        (world.editor, Role::Editor),
        (world.ws_admin, Role::WorkspaceAdmin),
        (member2, Role::Viewer),
    ] {
        set_workspace_member(&mut t, workspace, who, role).await.map_err(err)?;
    }
    set_document_member(&mut t, document, member2, Role::Viewer).await.map_err(err)?;
    teams::create_team(&mut t, team, &slug(), "e2 team", world.owner).await.map_err(err)?;
    teams::add_team_member(&mut t, team, member, TeamRole::Editor, world.owner).await.map_err(err)?;
    teams::grant_to_team(&mut t, team, TeamGrant::Workspace(workspace, WorkspaceGrantRole::Viewer), world.owner).await.map_err(err)?;
    teams::set_owning_team(&mut t, OwnedResource::Workspace(workspace), Some(team), world.owner).await.map_err(err)?;
    teams::set_owning_team(&mut t, OwnedResource::Project(project.as_uuid()), Some(team), world.owner).await.map_err(err)?;
    set_document_team_grant(&mut t, document, team, Role::Viewer).await.map_err(err)?;
    let invitation = create_invitation(&mut t, team, &format!("{}@example.test", slug()), TeamRole::Viewer, world.owner, None).await.map_err(err)?;
    let editor_link = create_share_link(&mut t, document, ShareRole::Editor, None, world.owner, None).await.map_err(err)?;
    let viewer_link = create_share_link(&mut t, document, ShareRole::Viewer, None, world.owner, None).await.map_err(err)?;
    t.commit().await?;
    // Share sessions are minted from live links (archiving afterwards ends them, as in production).
    let share_editor = world.identity.share_cookie_value(&exchange_share_token(pool, &editor_link.secret).await.map_err(err)?);
    let share_viewer = world.identity.share_cookie_value(&exchange_share_token(pool, &viewer_link.secret).await.map_err(err)?);
    if prep != Prep::None {
        let mut t = begin_tenant_transaction(pool, world.a).await.map_err(err)?;
        match prep {
            Prep::ArchiveWorkspace => drop(transition_workspace(&mut t, workspace, Transition::Archive).await.map_err(err)?.ok_or("archive")?),
            Prep::ArchiveProject => drop(transition_project(&mut t, project, Transition::Archive).await.map_err(err)?.ok_or("archive")?),
            Prep::ArchiveDocument => drop(archive_document(&mut t, document).await.map_err(err)?.ok_or("archive")?),
            Prep::None => {}
        }
        t.commit().await?;
    }
    Ok(Fixture {
        workspace,
        project,
        document,
        team,
        member,
        member2,
        newcomer,
        link: viewer_link.id,
        invitation: invitation.id,
        share_editor,
        share_viewer,
    })
}

fn concrete(path: &str, tenant: TenantId, fixture: &Fixture) -> String {
    path.replace("{t}", &tenant.to_string())
        .replace("{w}", &fixture.workspace.to_string())
        .replace("{p}", &fixture.project.to_string())
        .replace("{d}", &fixture.document.to_string())
        .replace("{team}", &fixture.team.to_string())
        .replace("{m2}", &fixture.member2.to_string())
        .replace("{m}", &fixture.member.to_string())
        .replace("{link}", &fixture.link.to_string())
        .replace("{inv}", &fixture.invitation.to_string())
}

fn body(send: Send, fixture: &Fixture) -> Option<Value> {
    Some(match send {
        Send::Nothing => return None,
        Send::Token => json!({"token": "e2-unknown-token-0000000000000000000000000"}),
        Send::Name => json!({"name": "e2 renamed"}),
        Send::NewTenant => json!({"slug": slug(), "name": "e2 created tenant"}),
        Send::AddTenantMember => json!({"principal_id": fixture.newcomer.to_string(), "role": "viewer"}),
        Send::MemberUpdate => json!({"role": "tenant_admin"}),
        Send::NewTeam => json!({"slug": slug(), "name": "e2 created team"}),
        Send::AddTeamMember => json!({"principal_id": fixture.member2.to_string(), "role": "viewer"}),
        Send::RoleViewer => json!({"role": "viewer"}),
        Send::RoleEditor => json!({"role": "editor"}),
        Send::ShareLink => json!({"role": "viewer"}),
        Send::Invitation => json!({"email": format!("{}@example.test", slug()), "role": "viewer"}),
    })
}

/// The headers `who` sends: its credential, the tenant it acts in, and our Origin.
async fn credentials(world: &World, who: Who, fixture: &Fixture, fresh_session: bool) -> Fallible<Vec<(String, String)>> {
    let mut headers = vec![("origin".to_string(), ORIGIN.to_string())];
    let cookie = |name: &str, value: &str| (header::COOKIE.to_string(), format!("{name}={value}"));
    match who {
        Who::Anonymous => {}
        Who::ShareEditor => headers.push(cookie(SHARE_COOKIE, &fixture.share_editor)),
        Who::ShareViewer => headers.push(cookie(SHARE_COOKIE, &fixture.share_viewer)),
        _ if who.service() => headers.push((header::AUTHORIZATION.to_string(), format!("Bearer {}", world.service[&who]))),
        _ => {
            let token = if fresh_session {
                // Logout revokes the session it is sent with: give it a throwaway one.
                let principal = match who {
                    Who::Viewer => world.viewer,
                    Who::Commenter => world.commenter,
                    Who::Editor => world.editor,
                    Who::WorkspaceAdmin => world.ws_admin,
                    Who::TenantAdmin => world.admin,
                    Who::TenantOwner => world.owner,
                    Who::OtherTenant => world.b_owner,
                    _ => principal(&world.pool, "e2 throwaway").await?,
                };
                session(&world.pool, principal).await?
            } else {
                world.sessions[&who].clone()
            };
            headers.push(cookie(figcraft_cloud_auth::session_policy::COOKIE_NAME, &token));
            let tenant = if who == Who::OtherTenant { world.b } else { world.a };
            headers.push((TENANT_HEADER.to_string(), tenant.to_string()));
        }
    }
    Ok(headers)
}

// ---------------------------------------------------------------------------------------------
// S4-021: the matrix

#[tokio::test]
async fn every_route_and_caller_matches_the_expected_matrix() -> Fallible<()> {
    let Some(pool) = pool().await else { return Ok(()) };
    let world = world(&pool).await?;
    let shared = fixture(&world, Prep::None).await?;
    let mut failures = Vec::new();
    let mut cells = 0usize;
    for row in ROWS {
        let read_only = row.method == "GET" && row.prep == Prep::None;
        for who in WHO {
            let fresh;
            let fixture = if read_only {
                &shared
            } else {
                fresh = fixture(&world, row.prep).await?;
                &fresh
            };
            let headers = credentials(&world, who, fixture, row.kind == Kind::Auth(AuthRoute::Logout)).await?;
            let path = concrete(row.path, world.a, fixture);
            let (status, reply) = send(&world.app, row.method, &path, &headers, body(row.send, fixture).as_ref()).await;
            cells += 1;
            let want = expected(row, who);
            if !matches(want, status) {
                failures.push(format!(
                    "{who:?} {} {}: expected {}, got {status} {reply}",
                    row.method,
                    row.route,
                    want.map_or("2xx".into(), |c| c.to_string())
                ));
            }
        }
    }
    assert_eq!(cells, ROWS.len() * WHO.len());
    assert!(failures.is_empty(), "{} of {cells} cells differ:\n{}", failures.len(), failures.join("\n"));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// S4-022: cross-tenant

fn ids_in(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            out.insert(text.clone());
        }
        Value::Array(items) => items.iter().for_each(|item| ids_in(item, out)),
        Value::Object(map) => map.values().for_each(|item| ids_in(item, out)),
        _ => {}
    }
}

#[tokio::test]
async fn cross_tenant_resources_are_invisible_to_another_tenants_owner() -> Fallible<()> {
    let Some(pool) = pool().await else { return Ok(()) };
    let world = world(&pool).await?;
    let a = fixture(&world, Prep::None).await?;
    // B gets its own team so `/tenants/{B}/teams/{B team}/...` can be aimed at A's children.
    let b_team = e2();
    {
        let mut t = begin_tenant_transaction(&pool, world.b).await.map_err(err)?;
        teams::create_team(&mut t, b_team, &slug(), "e2 B team", world.b_owner).await.map_err(err)?;
        t.commit().await?;
    }
    let b_owner = {
        let mut headers = vec![("origin".to_string(), ORIGIN.to_string())];
        headers.push((
            header::COOKIE.to_string(),
            format!("{}={}", figcraft_cloud_auth::session_policy::COOKIE_NAME, world.sessions[&Who::OtherTenant]),
        ));
        headers.push((TENANT_HEADER.to_string(), world.b.to_string()));
        headers
    };
    // A random fixture shaped like A's: nothing in it exists anywhere.
    let ghost = Fixture {
        workspace: e2_id(),
        project: e2_id(),
        document: e2_id(),
        team: e2(),
        member: e2_id(),
        member2: e2_id(),
        newcomer: e2_id(),
        link: e2(),
        invitation: e2(),
        share_editor: String::new(),
        share_viewer: String::new(),
    };
    let ghost_tenant: TenantId = e2_id();

    // 1. Every route naming A's resources, or A's tenant, answers B's owner exactly as it answers
    //    for identifiers that do not exist: 404 for resources, the uniform 403 for A's tenant.
    for row in ROWS.iter().filter(|row| matches!(row.kind, Kind::Resource | Kind::TenantPath { .. })) {
        let real = send(&world.app, row.method, &concrete(row.path, world.a, &a), &b_owner, body(row.send, &a).as_ref()).await;
        let fake = send(&world.app, row.method, &concrete(row.path, ghost_tenant, &ghost), &b_owner, body(row.send, &ghost).as_ref()).await;
        let want = if row.kind == Kind::Resource { 404 } else { 403 };
        assert_eq!(real.0, want, "B owner on A's {} {}: {}", row.method, row.route, real.1);
        assert_eq!(real, fake, "{} {}: A's identifiers are distinguishable from unknown ones", row.method, row.route);
    }

    // 2. B's own tenant path aimed at A's children (team, member, workspace, project,
    //    invitation): 404, the same as for unknown children.
    let b_paths: &[(&str, &str, Send)] = &[
        ("GET", "/tenants/{t}/teams/{team}", Send::Nothing),
        ("PATCH", "/tenants/{t}/teams/{team}", Send::Name),
        ("DELETE", "/tenants/{t}/teams/{team}", Send::Nothing),
        ("GET", "/tenants/{t}/teams/{team}/members", Send::Nothing),
        ("POST", "/tenants/{t}/teams/{team}/members", Send::AddTeamMember),
        ("GET", "/tenants/{t}/teams/{team}/invitations", Send::Nothing),
        ("PATCH", "/tenants/{t}/members/{m}", Send::MemberUpdate),
        ("DELETE", "/tenants/{t}/members/{m}", Send::Nothing),
        ("GET", "/tenants/{t}/members/{m}/permissions", Send::Nothing),
        ("PUT", "/tenants/{t}/members/{m}/permissions/provider_keys", Send::Nothing),
        ("DELETE", "/tenants/{t}/members/{m}/permissions/support", Send::Nothing),
    ];
    let b_children: &[(&str, &str, Send)] = &[
        ("PATCH", "/tenants/{t}/teams/{bt}/members/{m}", Send::RoleViewer),
        ("DELETE", "/tenants/{t}/teams/{bt}/members/{m}", Send::Nothing),
        ("PUT", "/tenants/{t}/teams/{bt}/grants/workspaces/{w}", Send::RoleEditor),
        ("DELETE", "/tenants/{t}/teams/{bt}/grants/workspaces/{w}", Send::Nothing),
        ("PUT", "/tenants/{t}/teams/{bt}/owned/workspaces/{w}", Send::Nothing),
        ("DELETE", "/tenants/{t}/teams/{bt}/owned/workspaces/{w}", Send::Nothing),
        ("PUT", "/tenants/{t}/teams/{bt}/owned/projects/{p}", Send::Nothing),
        ("DELETE", "/tenants/{t}/teams/{bt}/owned/projects/{p}", Send::Nothing),
        ("DELETE", "/tenants/{t}/teams/{bt}/invitations/{inv}", Send::Nothing),
    ];
    for (method, path, send_body) in b_paths.iter().chain(b_children) {
        let path = path.replace("{bt}", &b_team.to_string());
        let real = send(&world.app, method, &concrete(&path, world.b, &a), &b_owner, body(*send_body, &a).as_ref()).await;
        let fake = send(&world.app, method, &concrete(&path, world.b, &ghost), &b_owner, body(*send_body, &ghost).as_ref()).await;
        assert_eq!(real.0, 404, "B owner, B path, A's child: {method} {path}: {}", real.1);
        assert_eq!(real, fake, "{method} {path}: A's child is distinguishable from an unknown one");
    }
    // A's team, members and grants can't be pulled into B either: adding A's member to B's team
    // or granting B's team on A's document is a 404 / refusal like an unknown principal.
    let add_a_member = json!({"principal_id": a.member.to_string(), "role": "viewer"});
    let (status, _) = send(&world.app, "POST", &format!("/tenants/{}/teams/{b_team}/members", world.b), &b_owner, Some(&add_a_member)).await;
    assert!(matches!(status, 404 | 409), "adding A's member to B's team: {status}");

    // 3. Lists never include A's identifiers.
    let a_ids: BTreeSet<String> = [
        world.a.to_string(),
        a.workspace.to_string(),
        a.project.to_string(),
        a.document.to_string(),
        a.team.to_string(),
        a.link.to_string(),
        a.invitation.to_string(),
    ]
    .into_iter()
    .collect();
    for path in [
        "/tenants".to_string(),
        "/workspaces".to_string(),
        format!("/tenants/{}/teams", world.b),
        format!("/tenants/{}/members", world.b),
        format!("/tenants/{}/invitations", world.b),
        format!("/tenants/{}/teams/{b_team}/members", world.b),
        format!("/tenants/{}/teams/{b_team}/invitations", world.b),
    ] {
        let (status, reply) = send(&world.app, "GET", &path, &b_owner, None).await;
        assert_eq!(status, 200, "{path}: {reply}");
        let mut seen = BTreeSet::new();
        ids_in(&reply, &mut seen);
        assert!(seen.is_disjoint(&a_ids), "{path} lists A's identifiers: {reply}");
    }
    // B's workspace list in B never carries A's workspace even after B creates its own.
    let (status, _) = send(&world.app, "POST", "/workspaces", &b_owner, Some(&json!({"name": "e2 B workspace"}))).await;
    assert_eq!(status, 201);
    let (_, reply) = send(&world.app, "GET", "/workspaces", &b_owner, None).await;
    assert!(!reply.to_string().contains(&a.workspace.to_string()));

    // 4. Object routes: the router has no download or presign route (the sweep in
    //    `every_route_is_listed_and_nothing_else_is_registered` holds that); the one route that
    //    mints a credential, collab-link, refuses A's document for B (above) and for A's own
    //    share session presented with B's tenant.
    let share_in_b = vec![
        ("origin".to_string(), ORIGIN.to_string()),
        (header::COOKIE.to_string(), format!("{SHARE_COOKIE}={}", a.share_editor)),
        (TENANT_HEADER.to_string(), world.b.to_string()),
    ];
    let (status, _) = send(&world.app, "GET", &format!("/documents/{}/collab-link", a.document), &share_in_b, None).await;
    assert_eq!(status, 403);

    // 5. Nothing of A's changed: A's owner still reads every resource with its original name.
    let a_owner = vec![
        ("origin".to_string(), ORIGIN.to_string()),
        (header::COOKIE.to_string(), format!("{}={}", figcraft_cloud_auth::session_policy::COOKIE_NAME, world.sessions[&Who::TenantOwner])),
        (TENANT_HEADER.to_string(), world.a.to_string()),
    ];
    for (path, name) in [
        (format!("/workspaces/{}", a.workspace), "e2 workspace"),
        (format!("/projects/{}", a.project), "e2 project"),
        (format!("/documents/{}", a.document), "e2 document"),
        (format!("/tenants/{}/teams/{}", world.a, a.team), "e2 team"),
    ] {
        let (status, reply) = send(&world.app, "GET", &path, &a_owner, None).await;
        assert_eq!((status, reply["name"].as_str()), (200, Some(name)), "{path}: {reply}");
        assert_eq!(reply["state"].as_str(), Some("active"), "{path}: {reply}");
    }
    let (_, members) = send(&world.app, "GET", &format!("/tenants/{}/teams/{}/members", world.a, a.team), &a_owner, None).await;
    assert!(members.to_string().contains(&a.member.to_string()), "{members}");
    let (_, permissions) = send(&world.app, "GET", &format!("/tenants/{}/members/{}/permissions", world.a, a.member), &a_owner, None).await;
    assert!(permissions.to_string().contains("support") && !permissions.to_string().contains("provider_keys"), "{permissions}");
    let (_, links) = send(&world.app, "GET", &format!("/documents/{}/share-links", a.document), &a_owner, None).await;
    assert!(links.to_string().contains(&a.link.to_string()), "{links}");
    let (_, invitations) = send(&world.app, "GET", &format!("/tenants/{}/teams/{}/invitations", world.a, a.team), &a_owner, None).await;
    assert!(invitations.to_string().contains(&a.invitation.to_string()), "{invitations}");
    Ok(())
}
