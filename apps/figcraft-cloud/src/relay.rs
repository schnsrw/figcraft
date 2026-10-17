//! The collaboration relay (core slice): FigCraft's collaboration protocol over WebSocket on port
//! 8082, every document in PostgreSQL and S3 (`figcraft_cloud_relay::collab_backend`), and the
//! development bootstrap that makes a share link to join it with.
//!
//! The gateway routes `/ws/v1/collab` (and below) here with the path kept: the hub answers a
//! WebSocket upgrade on any path, so no prefix needs stripping, and an invite link
//! `ws://<host>/ws/v1/collab?file=<document>&token=<secret>` names the endpoint by that path.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REVOCATION_INTERVAL, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::{create_document, get_document};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::Hosting;

use crate::config::CloudConfig;

/// Where the relay listens (inside the container; the gateway reaches it as `cloud:8082`).
pub(crate) const RELAY_ADDR: ([u8; 4], u16) = ([0, 0, 0, 0], 8082);
/// The path the gateway serves the relay under.
pub(crate) const RELAY_PATH: &str = "/ws/v1/collab";
/// The browser editor's path on the gateway.
pub(crate) const EDITOR_PATH: &str = "/edit/";
const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_TENANT: &str = "00000000-0000-0000-0000-000000000001";
/// The development owner every bootstrap link is created by (single-tenant development mode).
const DEVELOPMENT_PRINCIPAL: &str = "00000000-0000-4000-8000-00000000c0de";
const DEVELOPMENT_WORKSPACE: &str = "00000000-0000-4000-8000-00000000c0d1";
const DEVELOPMENT_PROJECT: &str = "00000000-0000-4000-8000-00000000c0d2";

/// Transport limits for the cloud relay: every connection arrives through the gateway (one
/// address), so the per-address cap is off and the totals bound the process.
pub(crate) fn net_limits() -> NetLimits {
    NetLimits {
        max_connections: 1024,
        max_per_address: 0,
        max_queued_total: 1 << 30,
        max_reading_total: 512 << 20,
        max_in_flight: 512 << 20,
        reserve_for_sessions: 64,
        ..NetLimits::default()
    }
}

/// A running relay: the hub and the sweeper that disconnects revoked share links (S4-017). Both
/// stop when this is dropped, so the caller keeps it for the life of the process.
pub(crate) struct Relay {
    pub(crate) hosting: Hosting,
    _revocations: RevocationSweeper,
}

/// Start the relay on [`RELAY_ADDR`] with storage from `config`, and its revocation sweeper.
pub(crate) fn start(config: &CloudConfig) -> Result<Relay, &'static str> {
    let database_url = config.database_url().ok_or("the relay requires DATABASE_URL")?;
    let objects = config.object_store_settings().ok_or("the relay requires the S3 settings")?;
    let mut backend_config = CloudBackendConfig::new(database_url, objects);
    // Realtime tickets are verified with the subkey of the same SESSION_SIGNING_KEY the API
    // mints them with; without it ticket hellos are refused.
    backend_config.ticket_key =
        config.session_signing_key().and_then(|secret| figcraft_cloud_storage::realtime_tickets::TicketKey::derive(secret.as_bytes()).ok());
    // Room leases (one replica serves each room, others refuse it) are on when this replica says
    // where it can be reached; a single relay needs none.
    backend_config.leases = config.relay_advertise_url().map(figcraft_cloud_relay::collab_backend::LeaseConfig::new);
    let backend = CloudBackend::connect(&backend_config).map_err(|_| "the relay's storage couldn't be set up")?;
    let watch = backend.revocation_watch();
    let listener = TcpListener::bind(SocketAddr::from(RELAY_ADDR)).map_err(|_| "the relay's port couldn't be opened")?;
    let hosting = Hosting::serve(listener, backend, net_limits()).map_err(|_| "the relay couldn't start")?;
    let revocations = RevocationSweeper::start(&backend_config, watch, hosting.addr(), REVOCATION_INTERVAL)
        .map_err(|_| "the relay's revocation sweeper couldn't start")?;
    Ok(Relay { hosting, _revocations: revocations })
}

/// Percent-encode everything but RFC 3986 unreserved characters (a URL inside a fragment).
pub(crate) fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The browser URL that opens the editor and joins `document` with `secret`.
pub(crate) fn editor_url(public_url: &str, document: CloudDocumentId, secret: &str) -> Result<String, &'static str> {
    let base = public_url.trim_end_matches('/');
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err("FIGCRAFT_PUBLIC_URL must start with http:// or https://");
    };
    let invite = format!("{socket}{RELAY_PATH}?file={document}&token={secret}");
    Ok(format!("{base}{EDITOR_PATH}#join={}", encode_component(&invite)))
}

fn setting(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

/// `--create-share-link`: make sure the development tenant (and, without `document`, a new
/// document) exists, create a link of `role`, and print the editor URL that joins with it.
pub(crate) fn create_link(config: &CloudConfig, document: Option<CloudDocumentId>, role: ShareRole) -> Result<String, &'static str> {
    let database_url = config.database_url().ok_or("DATABASE_URL or DATABASE_URL_FILE is required")?;
    let tenant: TenantId = setting("FIGCRAFT_SINGLE_TENANT_ID", DEFAULT_TENANT).parse().map_err(|_| "FIGCRAFT_SINGLE_TENANT_ID must be a UUID")?;
    let public_url = setting("FIGCRAFT_PUBLIC_URL", DEFAULT_PUBLIC_URL);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| "unable to initialize the runtime")?;
    let (document, secret) = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), bootstrap(database_url, tenant, document, role)).await.map_err(|_| "the database timed out")?
    })?;
    editor_url(&public_url, document, &secret)
}

async fn bootstrap(
    database_url: &str,
    tenant: TenantId,
    document: Option<CloudDocumentId>,
    role: ShareRole,
) -> Result<(CloudDocumentId, String), &'static str> {
    const DATABASE: &str = "the database refused the bootstrap";
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(database_url).await.map_err(|_| "database connection failed")?;
    let principal: PrincipalId = DEVELOPMENT_PRINCIPAL.parse().map_err(|_| DATABASE)?;
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|_| DATABASE)?;
    let statements = [
        "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'service', 'FigCraft development owner') ON CONFLICT DO NOTHING",
        "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, 'development', 'Development', 'development', 'local') ON CONFLICT DO NOTHING",
        "INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, 'tenant_owner', 'active') ON CONFLICT DO NOTHING",
    ];
    for statement in statements {
        sqlx::query(statement).bind(tenant.to_string()).bind(principal.to_string()).execute(transaction.connection()).await.map_err(|_| DATABASE)?;
    }
    let document = match document {
        Some(id) => {
            get_document(&mut transaction, id).await.map_err(|_| DATABASE)?.ok_or("no such document in the development tenant")?;
            id
        }
        None => {
            let workspace: WorkspaceId = DEVELOPMENT_WORKSPACE.parse().map_err(|_| DATABASE)?;
            let project: ProjectId = DEVELOPMENT_PROJECT.parse().map_err(|_| DATABASE)?;
            sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'Development') ON CONFLICT DO NOTHING")
                .bind(workspace.to_string())
                .bind(tenant.to_string())
                .execute(transaction.connection())
                .await
                .map_err(|_| DATABASE)?;
            sqlx::query(
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'Drafts') ON CONFLICT DO NOTHING",
            )
            .bind(project.to_string())
            .bind(tenant.to_string())
            .bind(workspace.to_string())
            .execute(transaction.connection())
            .await
            .map_err(|_| DATABASE)?;
            let id = CloudDocumentId::random();
            create_document(&mut transaction, id, workspace, project, "Untitled").await.map_err(|_| DATABASE)?;
            id
        }
    };
    let link = create_share_link(&mut transaction, document, role, Some("development bootstrap"), principal, None).await.map_err(|_| DATABASE)?;
    transaction.commit().await.map_err(|_| DATABASE)?;
    Ok((document, link.secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_urls_carry_an_encoded_invite() {
        let document: CloudDocumentId = "b1300000-0000-4000-8000-000000000001".parse().unwrap();
        let url = editor_url("http://127.0.0.1:8080/", document, "abc-_XYZ").unwrap();
        assert_eq!(
            url,
            "http://127.0.0.1:8080/edit/#join=ws%3A%2F%2F127.0.0.1%3A8080%2Fws%2Fv1%2Fcollab%3Ffile%3Db1300000-0000-4000-8000-000000000001%26token%3Dabc-_XYZ"
        );
        let secure = editor_url("https://figcraft.example", document, "t").unwrap();
        assert!(secure.starts_with("https://figcraft.example/edit/#join=wss%3A%2F%2Ffigcraft.example%2Fws%2Fv1%2Fcollab"));
        assert!(editor_url("ftp://x", document, "t").is_err());
    }

    #[test]
    fn invites_in_editor_urls_parse_as_collab_invites() {
        let document: CloudDocumentId = "b1300000-0000-4000-8000-000000000002".parse().unwrap();
        let invite = format!("ws://127.0.0.1:8080{RELAY_PATH}?file={document}&token=AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde");
        let parsed = figcraft_collab_net::Invite::parse(&invite).unwrap();
        assert_eq!(parsed.file, document.to_string());
        assert_eq!(parsed.token, "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde");
        // Clients connect to the path with a trailing slash, which the gateway routes too.
        assert_eq!(parsed.server.path, format!("{RELAY_PATH}/"));
    }
}
