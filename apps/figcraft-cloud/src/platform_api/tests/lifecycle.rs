//! What members and outsiders see over HTTP while the platform moves a workspace through its
//! lifecycle (S6-047). The storage-level matrix over every other surface is
//! `crates/cloud-storage/tests/workspace_lifecycle.rs`.

use axum::http::{Method, StatusCode};
use serde_json::json;

use super::call;
use super::workspaces::{operator, slug, tenant_of, user};

#[tokio::test]
async fn members_of_a_non_active_workspace_are_told_so_and_outsiders_learn_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let Some(op) = operator().await else { return Ok(()) };
    let owner = user(&op.pool).await;
    let outsider = user(&op.pool).await;
    let created = op
        .call(
            Method::POST,
            "/platform/v1/workspaces",
            Some(json!({"slug": slug("sus"), "name": "P3 Http", "owner": {"principal_id": owner.to_string()}})),
        )
        .await;
    let tenant = tenant_of(&created);
    let limits = figcraft_cloud_storage::sessions::SessionLimits::default();
    let member = figcraft_cloud_storage::sessions::create_session(&op.pool, owner, limits).await.map_err(|e| format!("{e:?}"))?.token;
    let stranger = figcraft_cloud_storage::sessions::create_session(&op.pool, outsider, limits).await.map_err(|e| format!("{e:?}"))?.token;
    let as_member = format!("__Host-figcraft_session={member}");
    let as_stranger = format!("__Host-figcraft_session={stranger}");
    let path = format!("/api/v1/tenants/{tenant}");

    assert_eq!(call(&op.app, Method::GET, &path, Some(&as_member), None).await.status, StatusCode::OK);
    assert_eq!(op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/suspend"), Some(json!({}))).await.status, StatusCode::OK);
    for state in ["suspended", "archived", "deleting"] {
        if state != "suspended" {
            let step = if state == "archived" { "archive" } else { "request-deletion" };
            assert_eq!(op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/{step}"), Some(json!({}))).await.status, StatusCode::OK);
        }
        let read = call(&op.app, Method::GET, &path, Some(&as_member), None).await;
        assert_eq!((read.status, read.code()), (StatusCode::CONFLICT, "resource_inactive"), "{state}");
        let write = call(&op.app, Method::PATCH, &path, Some(&as_member), Some(json!({"name": "renamed"}))).await;
        assert_eq!((write.status, write.code()), (StatusCode::CONFLICT, "resource_inactive"), "{state} writes");
        let stranger_reply = call(&op.app, Method::GET, &path, Some(&as_stranger), None).await;
        assert_eq!((stranger_reply.status, stranger_reply.code()), (StatusCode::FORBIDDEN, "forbidden"), "{state}: outsiders cannot tell");
        // The member's picker still lists the workspace with its state, until deletion is pending.
        let listing = call(&op.app, Method::GET, "/api/v1/me/workspaces", Some(&as_member), None).await;
        assert_eq!(listing.status, StatusCode::OK);
        let listed =
            listing.body["items"].as_array().ok_or("items")?.iter().find(|t| t["id"] == json!(tenant.to_string())).map(|t| t["state"].clone());
        assert_eq!(listed, if state == "deleting" { None } else { Some(json!(state)) }, "{state}");
        let outsider_listing = call(&op.app, Method::GET, "/api/v1/me/workspaces", Some(&as_stranger), None).await;
        assert!(outsider_listing.body["items"].as_array().ok_or("items")?.is_empty());
    }
    assert_eq!(op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/cancel-deletion"), None).await.status, StatusCode::OK);
    assert_eq!(op.call(Method::POST, &format!("/platform/v1/workspaces/{tenant}/restore"), None).await.status, StatusCode::OK);
    assert_eq!(call(&op.app, Method::GET, &path, Some(&as_member), None).await.status, StatusCode::OK, "restored");
    assert_eq!(call(&op.app, Method::GET, &path, Some(&as_stranger), None).await.status, StatusCode::FORBIDDEN);
    op.done().await;
    Ok(())
}
