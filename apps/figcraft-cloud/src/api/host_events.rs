//! Webhook events for document changes (S6-015).
//!
//! Called inside the transaction that changes a document, from the host routes and from the
//! ordinary document routes, so an archive done by a person in the browser reaches the host that
//! embeds the document just like one the host asked for. Only documents some host has mapped
//! produce events, and each mapping host gets the event with its own external id.

use figcraft_cloud_domain::ErrorCode;
use figcraft_cloud_storage::TenantTransaction;
use figcraft_cloud_storage::metadata::DocumentRecord;
use figcraft_cloud_storage::webhook_events::{document_hosts, publish_event};

use super::error::ApiError;

/// Publish `event_type` for `document` to every host that has mapped it. The payload holds ids
/// only (see `figcraft_cloud_storage::webhook_events`).
pub(crate) async fn publish_document_event(
    transaction: &mut TenantTransaction<'_>,
    event_type: &str,
    document: &DocumentRecord,
) -> Result<(), ApiError> {
    let hosts = document_hosts(transaction, document.id.as_uuid()).await.map_err(|_| ApiError::unavailable())?;
    let (document_id, workspace_id, project_id) = (document.id.to_string(), document.workspace_id.to_string(), document.project_id.to_string());
    for (system_id, external_id) in hosts {
        let data = [
            ("document_id", document_id.as_str()),
            ("external_document_id", external_id.as_str()),
            ("workspace_id", workspace_id.as_str()),
            ("project_id", project_id.as_str()),
        ];
        publish_event(transaction, &system_id, event_type, &data).await.map_err(|_| ApiError::new(ErrorCode::Internal))?;
    }
    Ok(())
}
