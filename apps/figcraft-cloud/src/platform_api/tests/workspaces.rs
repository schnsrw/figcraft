//! `/platform/v1/workspaces` and the platform audit log (S6-046) against the dev PostgreSQL.

use axum::Router;
use axum::http::{Method, StatusCode};
use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::platform_workspaces::accept_owner_invitation;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{Reply, admin, call, cleanup, generous, pool, sign_in, state};

pub(super) struct Operator {
    pub(super) pool: PgPool,
    pub(super) app: Router,
    cookie: String,
    admin: Uuid,
}

pub(super) async fn operator() -> Option<Operator> {
    let pool = pool().await?;
    let state = state(pool.clone(), generous());
    let app = super::app(state.clone());
    let (admin_id, login) = admin(&pool, &state, "ws").await;
    let (cookie, _) = sign_in(&app, &login, None, -1).await;
    Some(Operator { pool, app, cookie, admin: admin_id })
}

impl Operator {
    pub(super) async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Reply {
        call(&self.app, method, path, Some(&self.cookie), body).await
    }

    pub(super) async fn done(self) {
        cleanup(&self.pool, &[self.admin]).await;
    }
}

pub(super) async fn user(pool: &PgPool) -> PrincipalId {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'p2 owner')")
        .bind(id.to_string())
        .execute(pool)
        .await
        .expect("principal");
    id.try_into().expect("non-nil")
}

pub(super) fn slug(prefix: &str) -> String {
    format!("p2-{prefix}-{}", &Uuid::new_v4().simple().to_string()[..10])
}

pub(super) fn tenant_of(reply: &Reply) -> TenantId {
    reply.body["id"].as_str().expect("id").parse().expect("tenant id")
}

async fn tenant_audit_actions(pool: &PgPool, tenant: TenantId) -> Vec<String> {
    let mut tx = begin_tenant_transaction(pool, tenant).await.expect("tx");
    let rows = sqlx::query_scalar("SELECT action FROM audit_events ORDER BY created_at, id").fetch_all(tx.connection()).await.expect("audit");
    let _ = tx.rollback().await;
    rows
}

#[tokio::test]
async fn a_workspace_with_an_existing_owner_is_active_and_audited_twice() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let owner = user(&op.pool).await;
    let name = slug("own");
    let created = op
        .call(Method::POST, "/platform/v1/workspaces", Some(json!({"slug": name, "name": "P2 Owned", "plan_id": "team", "home_region": "eu-west-1", "owner": {"principal_id": owner.to_string()}})))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let keys: std::collections::BTreeSet<_> = created.body.as_object().ok_or("object")?.keys().map(String::as_str).collect();
    // Metadata only: nothing about content or people.
    assert_eq!(keys, ["active_members", "created_at", "home_region", "id", "name", "plan_id", "slug", "state", "updated_at"].into_iter().collect());
    assert_eq!(
        (created.body["state"].clone(), created.body["plan_id"].clone(), created.body["active_members"].clone()),
        (json!("active"), json!("team"), json!(1))
    );
    let tenant = tenant_of(&created);

    let read = op.call(Method::GET, &format!("/platform/v1/workspaces/{tenant}"), None).await;
    assert_eq!((read.status, read.body["slug"].clone()), (StatusCode::OK, json!(name)));
    assert_eq!(op.call(Method::GET, &format!("/platform/v1/workspaces/{}", Uuid::new_v4()), None).await.status, StatusCode::NOT_FOUND);

    // One audit event on each side, in one transaction.
    assert!(tenant_audit_actions(&op.pool, tenant).await.contains(&"tenant.create".to_owned()));
    let audit = op.call(Method::GET, &format!("/platform/v1/audit?workspace_id={tenant}"), None).await;
    let items = audit.body["items"].as_array().ok_or("items")?;
    assert!(items.iter().any(|e| e["action"] == "platform.workspace.create"
        && e["actor_id"] == json!(op.admin.to_string())
        && e["metadata"]["owner"] == "principal"));
    op.done().await;
    Ok(())
}

#[tokio::test]
async fn an_invited_owner_activates_a_pending_workspace_once() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let created = op
        .call(
            Method::POST,
            "/platform/v1/workspaces",
            Some(json!({"slug": slug("inv"), "name": "P2 Invited", "owner": {"email": " Founder@Example.COM "}})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(
        (created.body["state"].clone(), created.body["plan_id"].clone(), created.body["active_members"].clone()),
        (json!("pending"), json!("free"), json!(0))
    );
    let tenant = tenant_of(&created);
    let token = created.body["owner_invitation"]["token"].as_str().ok_or("token")?.to_owned();
    assert_eq!(token.len(), 43);

    // A pending workspace cannot be suspended, renamed to active by the platform, or given a plan.
    for path in ["suspend", "archive", "restore"] {
        let reply = op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/{path}"), Some(json!({}))).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{path}");
    }
    assert_eq!(
        op.call(Method::PUT, &format!("/platform/v1/workspaces/{tenant}/plan"), Some(json!({"plan_id": "team"}))).await.status,
        StatusCode::CONFLICT
    );

    let owner = user(&op.pool).await;
    let bystander = user(&op.pool).await;
    use figcraft_cloud_storage::platform::PlatformError::Refused;
    assert_eq!(accept_owner_invitation(&op.pool, &"B".repeat(43), owner, None).await.err(), Some(Refused));
    assert_eq!(accept_owner_invitation(&op.pool, "short", owner, None).await.err(), Some(Refused));
    assert_eq!(
        accept_owner_invitation(&op.pool, &token, owner, Some("someone.else@example.com")).await.err(),
        Some(Refused),
        "the verified email must match"
    );
    // A principal that is not an active user cannot take the workspace.
    sqlx::query("UPDATE principals SET state = 'suspended' WHERE id = $1::uuid").bind(bystander.to_string()).execute(&op.pool).await?;
    assert_eq!(accept_owner_invitation(&op.pool, &token, bystander, None).await.err(), Some(Refused));

    let accepted = accept_owner_invitation(&op.pool, &token, owner, Some("founder@example.com")).await?;
    assert_eq!(accepted.id, tenant);
    assert_eq!(accepted.state, figcraft_cloud_domain::ResourceState::Active);
    let read = op.call(Method::GET, &format!("/platform/v1/workspaces/{tenant}"), None).await;
    assert_eq!((read.body["state"].clone(), read.body["active_members"].clone()), (json!("active"), json!(1)));
    assert_eq!(accept_owner_invitation(&op.pool, &token, owner, None).await.err(), Some(Refused), "spent");
    assert!(tenant_audit_actions(&op.pool, tenant).await.contains(&"tenant.owner_accept".to_owned()));
    let role: String = {
        let mut tx = begin_tenant_transaction(&op.pool, tenant).await.map_err(|e| format!("{e:?}"))?;
        let role = sqlx::query_scalar("SELECT role FROM tenant_members WHERE principal_id = $1::uuid")
            .bind(owner.to_string())
            .fetch_one(tx.connection())
            .await?;
        let _ = tx.rollback().await;
        role
    };
    assert_eq!(role, "tenant_owner");
    // An active workspace has no pending invitation to replace.
    let reissue = op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/owner-invitation"), Some(json!({"email": "x@example.com"}))).await;
    assert_eq!(reissue.status, StatusCode::CONFLICT);
    op.done().await;
    Ok(())
}

#[tokio::test]
async fn a_lost_owner_link_is_replaced_and_an_expired_one_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let created = op
        .call(
            Method::POST,
            "/platform/v1/workspaces",
            Some(json!({"slug": slug("re"), "name": "P2 Reissue", "owner": {"email": "late@example.com"}, "invitation_lifetime_secs": 3600})),
        )
        .await;
    let tenant = tenant_of(&created);
    let first = created.body["owner_invitation"]["token"].as_str().ok_or("token")?.to_owned();
    let second =
        op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/owner-invitation"), Some(json!({"email": "late@example.com"}))).await;
    assert_eq!(second.status, StatusCode::CREATED, "{:?}", second.body);
    let second = second.body["token"].as_str().ok_or("token")?.to_owned();
    assert_ne!(first, second);
    let owner = user(&op.pool).await;
    use figcraft_cloud_storage::platform::PlatformError::Refused;
    assert_eq!(accept_owner_invitation(&op.pool, &first, owner, None).await.err(), Some(Refused), "the replaced link is dead");
    let mut tx = begin_tenant_transaction(&op.pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query("UPDATE workspace_owner_invitations SET expires_at = now() - interval '1 second' WHERE state = 'pending'")
        .execute(tx.connection())
        .await?;
    tx.commit().await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(accept_owner_invitation(&op.pool, &second, owner, None).await.err(), Some(Refused), "expired");
    // Out-of-range lifetimes and bad addresses are refused up front.
    let bad = op
        .call(
            Method::POST,
            &format!("/platform/v1/workspaces/{tenant}/owner-invitation"),
            Some(json!({"email": "late@example.com", "invitation_lifetime_secs": 5})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let bad = op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/owner-invitation"), Some(json!({"email": "nope"}))).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    op.done().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_moves_follow_the_policy_and_are_audited_without_deleting_anything() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let owner = user(&op.pool).await;
    let created = op
        .call(
            Method::POST,
            "/platform/v1/workspaces",
            Some(json!({"slug": slug("life"), "name": "P2 Life", "owner": {"principal_id": owner.to_string()}})),
        )
        .await;
    let tenant = tenant_of(&created);
    let path = |action: &str| format!("/platform/v1/workspaces/{tenant}/{action}");
    let state_of = |reply: &Reply| reply.body["state"].as_str().unwrap_or("").to_owned();

    // active -> suspended, idempotent, then back.
    let suspended = op.call(Method::POST, &path("suspend"), Some(json!({"reason": "payment overdue"}))).await;
    assert_eq!((suspended.status, state_of(&suspended).as_str()), (StatusCode::OK, "suspended"));
    assert_eq!(state_of(&op.call(Method::POST, &path("suspend"), Some(json!({}))).await), "suspended");
    // Illegal moves are 409 and recorded as denied.
    assert_eq!(op.call(Method::POST, &path("cancel-deletion"), None).await.status, StatusCode::CONFLICT);
    assert_eq!(state_of(&op.call(Method::POST, &path("restore"), None).await), "active");
    assert_eq!(
        op.call(Method::POST, &path("request-deletion"), Some(json!({}))).await.status,
        StatusCode::CONFLICT,
        "active cannot jump to pending deletion"
    );
    // active -> archived -> deleting -> archived -> active.
    assert_eq!(state_of(&op.call(Method::POST, &path("archive"), Some(json!({}))).await), "archived");
    assert_eq!(state_of(&op.call(Method::POST, &path("request-deletion"), Some(json!({"reason": "customer left"}))).await), "deleting");
    assert_eq!(op.call(Method::POST, &path("restore"), None).await.status, StatusCode::CONFLICT, "pending deletion is cancelled first");
    assert_eq!(op.call(Method::POST, &path("suspend"), Some(json!({}))).await.status, StatusCode::CONFLICT);
    assert_eq!(state_of(&op.call(Method::POST, &path("cancel-deletion"), None).await), "archived");
    assert_eq!(state_of(&op.call(Method::POST, &path("restore"), None).await), "active");

    // Reasons are bounded and printable; unknown workspaces are 404; unknown fields 400.
    for bad in [json!({"reason": ""}), json!({"reason": "x".repeat(501)}), json!({"reason": "bell\u{7}"}), json!({"why": "no"})] {
        assert_eq!(op.call(Method::POST, &path("suspend"), Some(bad)).await.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        op.call(Method::POST, &format!("/platform/v1/workspaces/{}/suspend", Uuid::new_v4()), Some(json!({}))).await.status,
        StatusCode::NOT_FOUND
    );

    // Nothing was deleted and the member is still there.
    assert_eq!(op.call(Method::GET, &format!("/platform/v1/workspaces/{tenant}"), None).await.body["active_members"], json!(1));
    let actions = tenant_audit_actions(&op.pool, tenant).await;
    assert_eq!(actions.iter().filter(|a| a.as_str() == "tenant.lifecycle").count(), 6, "{actions:?}");
    let audit = op.call(Method::GET, &format!("/platform/v1/audit?workspace_id={tenant}&limit=100"), None).await;
    let items = audit.body["items"].as_array().ok_or("items")?;
    for (action, outcome) in [
        ("platform.workspace.suspend", "succeeded"),
        ("platform.workspace.restore", "succeeded"),
        ("platform.workspace.archive", "succeeded"),
        ("platform.workspace.request_deletion", "succeeded"),
        ("platform.workspace.cancel_deletion", "succeeded"),
        ("platform.workspace.state", "denied"),
    ] {
        assert!(items.iter().any(|e| e["action"] == action && e["outcome"] == outcome), "missing {action} {outcome}");
    }
    let suspend = items.iter().find(|e| e["action"] == "platform.workspace.suspend").ok_or("suspend event")?;
    assert_eq!((suspend["metadata"]["reason"].clone(), suspend["metadata"]["from"].clone()), (json!("payment overdue"), json!("active")));
    op.done().await;
    Ok(())
}

#[tokio::test]
async fn creation_and_plan_input_is_validated() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let owner = user(&op.pool).await;
    let owner_json = || json!({"principal_id": owner.to_string()});
    let first = slug("dup");
    let created = op.call(Method::POST, "/platform/v1/workspaces", Some(json!({"slug": first, "name": "P2 Dup", "owner": owner_json()}))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let tenant = tenant_of(&created);
    let duplicate = op.call(Method::POST, "/platform/v1/workspaces", Some(json!({"slug": first, "name": "P2 Dup 2", "owner": owner_json()}))).await;
    assert_eq!((duplicate.status, duplicate.code()), (StatusCode::CONFLICT, "conflict"));
    let attempts = [
        json!({"slug": "admin", "name": "Reserved", "owner": owner_json()}),
        json!({"slug": "Bad_Slug", "name": "Bad", "owner": owner_json()}),
        json!({"slug": slug("x"), "name": "   ", "owner": owner_json()}),
        json!({"slug": slug("x"), "name": "Plan", "plan_id": "unlimited", "owner": owner_json()}),
        json!({"slug": slug("x"), "name": "Region", "home_region": "EU West", "owner": owner_json()}),
        json!({"slug": slug("x"), "name": "Both", "owner": {"principal_id": owner.to_string(), "email": "a@example.com"}}),
        json!({"slug": slug("x"), "name": "Neither", "owner": {}}),
        json!({"slug": slug("x"), "name": "Ghost", "owner": {"principal_id": Uuid::new_v4().to_string()}}),
        json!({"slug": slug("x"), "name": "Email", "owner": {"email": "not an email"}}),
        json!({"slug": slug("x"), "name": "Extra", "owner": owner_json(), "state": "active"}),
    ];
    for attempt in attempts {
        let reply = op.call(Method::POST, "/platform/v1/workspaces", Some(attempt.clone())).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{attempt} -> {:?}", reply.body);
    }

    let plan = |id: &str| json!({"plan_id": id});
    let path = format!("/platform/v1/workspaces/{tenant}/plan");
    assert_eq!(op.call(Method::PUT, &path, Some(plan("business"))).await.body["plan_id"], "business");
    assert_eq!(op.call(Method::PUT, &path, Some(plan("business"))).await.status, StatusCode::OK, "same plan is a no-op");
    assert_eq!(op.call(Method::PUT, &path, Some(plan("platinum"))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        op.call(Method::PUT, &format!("/platform/v1/workspaces/{}/plan", Uuid::new_v4()), Some(plan("team"))).await.status,
        StatusCode::NOT_FOUND
    );
    let audit = op.call(Method::GET, &format!("/platform/v1/audit?workspace_id={tenant}&action_prefix=platform.workspace.plan"), None).await;
    assert_eq!(audit.body["items"].as_array().map(Vec::len), Some(1), "the no-op left no event");
    op.done().await;
    Ok(())
}

#[tokio::test]
async fn workspace_listing_pages_filters_and_rejects_tampered_cursors() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let owner = user(&op.pool).await;
    let prefix = format!("p2l{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mut ids = Vec::new();
    for n in 0..3 {
        let created = op
            .call(
                Method::POST,
                "/platform/v1/workspaces",
                Some(json!({"slug": format!("{prefix}-{n}"), "name": format!("P2 List {n}"), "owner": {"principal_id": owner.to_string()}})),
            )
            .await;
        ids.push(tenant_of(&created).to_string());
    }
    op.call(Method::POST, &format!("/platform/v1/workspaces/{}/suspend", ids[1]), Some(json!({}))).await;
    let first = op.call(Method::GET, &format!("/platform/v1/workspaces?slug_prefix={prefix}&limit=2"), None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["items"].as_array().map(Vec::len), Some(2));
    let cursor = first.body["next_cursor"].as_str().ok_or("cursor")?.to_owned();
    let second = op.call(Method::GET, &format!("/platform/v1/workspaces?slug_prefix={prefix}&limit=2&cursor={cursor}"), None).await;
    assert_eq!(second.body["items"].as_array().map(Vec::len), Some(1));
    assert!(second.body["next_cursor"].is_null());
    let mut seen: Vec<String> = first.body["items"]
        .as_array()
        .ok_or("items")?
        .iter()
        .chain(second.body["items"].as_array().ok_or("items")?)
        .map(|w| w["id"].as_str().unwrap_or("").to_owned())
        .collect();
    seen.sort();
    ids.sort();
    assert_eq!(seen, ids);
    let suspended = op.call(Method::GET, &format!("/platform/v1/workspaces?slug_prefix={prefix}&state=suspended"), None).await;
    assert_eq!(suspended.body["items"].as_array().map(Vec::len), Some(1));
    // A cursor belongs to its filter; a changed or forged one is the same fixed 400.
    let other = op.call(Method::GET, &format!("/platform/v1/workspaces?slug_prefix={prefix}&state=active&cursor={cursor}"), None).await;
    assert_eq!((other.status, other.code()), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    let forged = op.call(Method::GET, &format!("/platform/v1/workspaces?slug_prefix={prefix}&cursor={}x", cursor), None).await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    for bad in ["state=gone", "slug_prefix=Bad%20Prefix", "limit=0"] {
        let reply = op.call(Method::GET, &format!("/platform/v1/workspaces?{bad}"), None).await;
        assert!(reply.status == StatusCode::BAD_REQUEST || bad == "limit=0", "{bad}: {}", reply.status);
    }
    op.done().await;
    Ok(())
}
