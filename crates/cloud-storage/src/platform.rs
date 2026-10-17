//! The platform realm's database access (S6-044).
//!
//! Platform operators run the deployment; they are not workspace users. Their tables
//! (`platform_admins`, `platform_sessions`, `platform_bootstrap`, `platform_audit_events`) have no
//! tenant and are guarded by forced row-level security that opens only inside a
//! [`PlatformTransaction`]. A platform transaction never sets the tenant context, so it sees no
//! workspace data; a tenant transaction never sets the platform scope, so a workspace caller sees no
//! platform data. The platform reads workspaces only through [`list_workspaces`] and
//! [`get_workspace`], which return metadata (slug, name, state, plan, region, timestamps and the
//! active member count) and nothing that lives below a workspace.
//!
//! Callers outside this crate must authenticate the platform caller first (the `/platform/v1`
//! authenticator); this module does not decide who may act.

use figcraft_cloud_domain::{ResourceState, TenantId, Uuid};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use std::fmt;

use crate::metadata::{Page, PageSize, page, state};

/// Fixed, non-leaking categories for platform storage failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformError {
    /// Input failed validation (a bound, a character set, an unknown value).
    Invalid,
    NotFound,
    /// A uniqueness rule or a lifecycle rule refused the change.
    Conflict,
    /// The caller is refused (disabled admin, last admin, expired token).
    Refused,
    Database,
    CorruptRow,
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "platform error: {self:?}")
    }
}

impl std::error::Error for PlatformError {}

pub(crate) fn database_error(error: &sqlx::Error) -> PlatformError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => PlatformError::Conflict,
        Some("23514" | "22023" | "22001") => PlatformError::Invalid,
        _ => PlatformError::Database,
    }
}

/// A transaction with the platform scope set. Carries no tenant context.
pub struct PlatformTransaction<'a> {
    inner: Transaction<'a, Postgres>,
}

impl PlatformTransaction<'_> {
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    pub async fn commit(self) -> Result<(), PlatformError> {
        self.inner.commit().await.map_err(|error| database_error(&error))
    }

    pub async fn rollback(self) -> Result<(), PlatformError> {
        self.inner.rollback().await.map_err(|error| database_error(&error))
    }
}

/// Begin a transaction that can use the platform tables. The setting is transaction-local, so a
/// pooled connection never keeps it.
pub async fn begin_platform_transaction(pool: &PgPool) -> Result<PlatformTransaction<'_>, PlatformError> {
    let mut inner = pool.begin().await.map_err(|error| database_error(&error))?;
    sqlx::query("SELECT set_config('figcraft.platform_scope', 'on', true)").execute(&mut *inner).await.map_err(|error| database_error(&error))?;
    Ok(PlatformTransaction { inner })
}

/// Give a workspace transaction the platform scope too, for the few operations that must change a
/// workspace and the platform audit log atomically (create, suspend, restore, archive). It is the
/// only place besides [`begin_platform_transaction`] that sets the scope; its callers are the
/// functions of [`crate::platform_workspaces`].
pub async fn enter_platform_scope(transaction: &mut crate::TenantTransaction<'_>) -> Result<(), PlatformError> {
    sqlx::query("SELECT set_config('figcraft.platform_scope', 'on', true)")
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(())
}

/// Who did something on the platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformActor {
    /// A signed-in platform admin.
    Admin(Uuid),
    /// A provisioning credential (a host product's service account).
    Credential(Uuid),
    /// The process itself (a sweeper, a CLI command).
    System,
    /// Nobody was authenticated (a refused sign-in).
    Anonymous,
}

impl PlatformActor {
    const fn kind(self) -> &'static str {
        match self {
            Self::Admin(_) => "admin",
            Self::Credential(_) => "credential",
            Self::System => "system",
            Self::Anonymous => "anonymous",
        }
    }

    const fn id(self) -> Option<Uuid> {
        match self {
            Self::Admin(id) | Self::Credential(id) => Some(id),
            Self::System | Self::Anonymous => None,
        }
    }
}

/// How an audited action ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformOutcome {
    Succeeded,
    Denied,
    Failed,
}

impl PlatformOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// One platform audit event to append.
#[derive(Clone, Debug)]
pub struct PlatformAudit<'a> {
    pub actor: PlatformActor,
    pub action: &'a str,
    pub resource_kind: &'a str,
    pub resource_id: Option<Uuid>,
    pub tenant_id: Option<TenantId>,
    pub outcome: PlatformOutcome,
    pub request_id: Option<&'a str>,
    /// Never secrets, passwords or tokens; at most 8 KiB.
    pub metadata: &'a serde_json::Value,
}

/// Append one event to the platform audit log (append-only: the runtime role cannot update or
/// delete it).
pub async fn record_platform_audit(transaction: &mut PlatformTransaction<'_>, event: &PlatformAudit<'_>) -> Result<Uuid, PlatformError> {
    record_platform_audit_on(transaction.connection(), event).await
}

/// [`record_platform_audit`] on a connection that already holds the platform scope (a
/// [`PlatformTransaction`], or a tenant transaction that called [`enter_platform_scope`]).
pub(crate) async fn record_platform_audit_on(connection: &mut PgConnection, event: &PlatformAudit<'_>) -> Result<Uuid, PlatformError> {
    let id = Uuid::new_v4();
    let metadata = serde_json::to_string(event.metadata).map_err(|_| PlatformError::Invalid)?;
    sqlx::query(
        "INSERT INTO platform_audit_events (id, actor_kind, actor_id, action, resource_kind, resource_id, tenant_id, outcome, request_id, metadata) \
         VALUES ($1::uuid, $2, $3::uuid, $4, $5, $6::uuid, $7::uuid, $8, $9, $10::jsonb)",
    )
    .bind(id.to_string())
    .bind(event.actor.kind())
    .bind(event.actor.id().map(|id| id.to_string()))
    .bind(event.action)
    .bind(event.resource_kind)
    .bind(event.resource_id.map(|id| id.to_string()))
    .bind(event.tenant_id.map(|id| id.to_string()))
    .bind(event.outcome.as_str())
    .bind(event.request_id)
    .bind(metadata)
    .execute(connection)
    .await
    .map_err(|error| database_error(&error))?;
    Ok(id)
}

/// A stored platform audit event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformAuditRecord {
    pub id: Uuid,
    /// Microseconds since the Unix epoch.
    pub created_at_micros: i64,
    pub actor_kind: String,
    pub actor_id: Option<Uuid>,
    pub action: String,
    pub resource_kind: String,
    pub resource_id: Option<Uuid>,
    pub tenant_id: Option<TenantId>,
    pub outcome: String,
    pub metadata: serde_json::Value,
}

/// The keyset position of an audit page: `(created_at_micros, id)` of the last row returned.
pub type AuditCursor = (i64, Uuid);

/// Optional filters for [`list_platform_audit`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AuditFilter<'a> {
    pub tenant_id: Option<TenantId>,
    pub action_prefix: Option<&'a str>,
}

type AuditRow = (String, i64, String, Option<String>, String, String, Option<String>, Option<String>, String, String);

/// Newest-first page of the platform audit log, at most `size` rows.
pub async fn list_platform_audit(
    transaction: &mut PlatformTransaction<'_>,
    after: Option<AuditCursor>,
    size: PageSize,
    filter: AuditFilter<'_>,
) -> Result<Page<PlatformAuditRecord, AuditCursor>, PlatformError> {
    if filter
        .action_prefix
        .is_some_and(|prefix| prefix.is_empty() || prefix.len() > 64 || !prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_'))
    {
        return Err(PlatformError::Invalid);
    }
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id::text, floor(extract(epoch FROM created_at) * 1000000)::bigint, actor_kind, actor_id::text, action, resource_kind, \
                resource_id::text, tenant_id::text, outcome, metadata::text \
         FROM platform_audit_events \
         WHERE ($1::bigint IS NULL OR (floor(extract(epoch FROM created_at) * 1000000)::bigint, id) < ($1::bigint, $2::uuid)) \
           AND ($3::uuid IS NULL OR tenant_id = $3::uuid) \
           AND ($4::text IS NULL OR left(action, char_length($4)) = $4) \
         ORDER BY created_at DESC, id DESC LIMIT $5",
    )
    .bind(after.map(|(micros, _)| micros))
    .bind(after.map(|(_, id)| id.to_string()))
    .bind(filter.tenant_id.map(|id| id.to_string()))
    .bind(filter.action_prefix)
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let items = rows
        .into_iter()
        .map(|(id, micros, actor_kind, actor_id, action, resource_kind, resource_id, tenant_id, outcome, metadata)| {
            let uuid = |value: &str| value.parse::<Uuid>().map_err(|_| PlatformError::CorruptRow);
            Ok(PlatformAuditRecord {
                id: uuid(&id)?,
                created_at_micros: micros,
                actor_kind,
                actor_id: actor_id.as_deref().map(uuid).transpose()?,
                action,
                resource_kind,
                resource_id: resource_id.as_deref().map(uuid).transpose()?,
                tenant_id: tenant_id.map(|id| id.parse().map_err(|_| PlatformError::CorruptRow)).transpose()?,
                outcome,
                metadata: serde_json::from_str(&metadata).map_err(|_| PlatformError::CorruptRow)?,
            })
        })
        .collect::<Result<Vec<_>, PlatformError>>()?;
    Ok(page(items, size, |item| (item.created_at_micros, item.id)))
}

/// What the platform may know about a workspace: metadata only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformWorkspace {
    pub id: TenantId,
    pub slug: String,
    pub name: String,
    pub state: ResourceState,
    pub plan_id: String,
    pub home_region: String,
    /// Unix seconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub active_members: i64,
}

type WorkspaceRow = (String, String, String, String, String, String, i64, i64, i64);

fn workspace(row: WorkspaceRow) -> Result<PlatformWorkspace, PlatformError> {
    Ok(PlatformWorkspace {
        id: row.0.parse().map_err(|_| PlatformError::CorruptRow)?,
        slug: row.1,
        name: row.2,
        state: state(&row.3).map_err(|_| PlatformError::CorruptRow)?,
        plan_id: row.4,
        home_region: row.5,
        created_at: row.6,
        updated_at: row.7,
        active_members: row.8,
    })
}

/// The wire/storage spelling of a lifecycle state.
#[must_use]
pub const fn state_name(state: ResourceState) -> &'static str {
    match state {
        ResourceState::Pending => "pending",
        ResourceState::Active => "active",
        ResourceState::Suspended => "suspended",
        ResourceState::Archived => "archived",
        ResourceState::Deleting => "deleting",
        ResourceState::Deleted => "deleted",
    }
}

/// Filters for [`list_workspaces`].
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceFilter<'a> {
    pub state: Option<ResourceState>,
    /// Lowercase slug prefix (letters, digits, hyphens), at most 63 characters.
    pub slug_prefix: Option<&'a str>,
}

/// A keyset page of workspaces by id, with the metadata the platform may see.
pub async fn list_workspaces(
    transaction: &mut PlatformTransaction<'_>,
    after: Option<TenantId>,
    size: PageSize,
    filter: WorkspaceFilter<'_>,
) -> Result<Page<PlatformWorkspace, TenantId>, PlatformError> {
    if filter.slug_prefix.is_some_and(|prefix| {
        prefix.is_empty() || prefix.len() > 63 || !prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }) {
        return Err(PlatformError::Invalid);
    }
    let rows: Vec<WorkspaceRow> = sqlx::query_as(
        "SELECT id, slug, name, state, plan_id, home_region, created_at, updated_at, active_members \
         FROM figcraft_platform_workspaces($1::uuid, $2::integer, $3, $4)",
    )
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit() as i32)
    .bind(filter.state.map(state_name))
    .bind(filter.slug_prefix)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let items = rows.into_iter().map(workspace).collect::<Result<Vec<_>, _>>()?;
    Ok(page(items, size, |item| item.id))
}

/// One workspace's metadata, or `None`.
pub async fn get_workspace(transaction: &mut PlatformTransaction<'_>, id: TenantId) -> Result<Option<PlatformWorkspace>, PlatformError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(
        "SELECT id, slug, name, state, plan_id, home_region, created_at, updated_at, active_members FROM figcraft_platform_workspace($1::uuid)",
    )
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    row.map(workspace).transpose()
}
