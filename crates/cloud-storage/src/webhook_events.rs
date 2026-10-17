//! Publishing events to webhook endpoints: subscription filtering and payload minimization
//! (S6-015).
//!
//! An event is published inside the transaction that made the change (a transactional outbox), so
//! a rolled-back change sends nothing and a committed one is never lost. [`publish_event`] queues
//! one delivery per endpoint of the host that is enabled and subscribed to the event type (an
//! endpoint with an empty subscription gets every event). The body is built by
//! [`minimal_body`]:
//!
//! ```json
//! {"id": "<event uuid>", "type": "document.archived", "created_at": 1790000000,
//!  "tenant_id": "<uuid>", "data": {"document_id": "<uuid>", "external_document_id": "...", ...}}
//! ```
//!
//! `data` may only hold the keys in [`DATA_KEYS`] (identifiers and a format name): never a
//! document or project name, file content, comment text, an email address or any other personal
//! data. A receiver that needs more calls the host API with the ids. Values are short printable
//! strings; anything else is refused rather than trimmed, so a careless caller cannot leak by
//! adding a field.

use figcraft_cloud_domain::{TenantId, Uuid};
use serde_json::{Map, Value, json};

use crate::TenantTransaction;
use crate::webhook_delivery::{DEFAULT_MAX_ATTEMPTS, DeliveryError, enqueue_delivery};
use crate::webhooks::EVENT_TYPES;

/// The only keys an event's `data` may contain.
pub const DATA_KEYS: &[&str] = &["document_id", "external_document_id", "workspace_id", "project_id", "export_id", "format"];
/// Longest value in `data`.
pub const MAX_DATA_VALUE_BYTES: usize = 512;
/// Hosts one document can be mapped by, as far as events fan out.
pub const MAX_HOSTS_PER_DOCUMENT: i64 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventError {
    /// Unknown event type, a key outside [`DATA_KEYS`], or an unsuitable value.
    InvalidInput,
    Database,
}

impl std::fmt::Display for EventError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "webhook event error: {self:?}")
    }
}

impl std::error::Error for EventError {}

impl From<DeliveryError> for EventError {
    fn from(error: DeliveryError) -> Self {
        match error {
            DeliveryError::InvalidInput | DeliveryError::NotFound => Self::InvalidInput,
            DeliveryError::Database => Self::Database,
        }
    }
}

fn plain_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_DATA_VALUE_BYTES && !value.chars().any(char::is_control)
}

/// Build the minimal event body, or refuse: the type must be a known event type and every `data`
/// key must be in [`DATA_KEYS`] (no duplicates) with a short printable value.
pub fn minimal_body(event_id: Uuid, event_type: &str, tenant: TenantId, created_at: i64, data: &[(&str, &str)]) -> Result<Vec<u8>, EventError> {
    if !EVENT_TYPES.contains(&event_type) {
        return Err(EventError::InvalidInput);
    }
    let mut fields = Map::new();
    for (key, value) in data {
        if !DATA_KEYS.contains(key) || !plain_value(value) || fields.contains_key(*key) {
            return Err(EventError::InvalidInput);
        }
        fields.insert((*key).to_owned(), Value::String((*value).to_owned()));
    }
    let body = json!({
        "id": event_id.to_string(),
        "type": event_type,
        "created_at": created_at,
        "tenant_id": tenant.to_string(),
        "data": Value::Object(fields),
    });
    serde_json::to_vec(&body).map_err(|_| EventError::InvalidInput)
}

/// Whether an endpoint subscribed to `subscription` receives `event_type` (empty: everything).
#[must_use]
pub fn subscribed(subscription: &[String], event_type: &str) -> bool {
    subscription.is_empty() || subscription.iter().any(|subscribed| subscribed == event_type)
}

/// Queue `event_type` for every enabled, subscribed endpoint of `system_id` (when the host itself
/// is enabled). Returns how many deliveries were queued. One event id is shared by all endpoints.
pub async fn publish_event(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    event_type: &str,
    data: &[(&str, &str)],
) -> Result<usize, EventError> {
    let event_id = Uuid::new_v4();
    let created_at: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(transaction.connection())
        .await
        .map_err(|_| EventError::Database)?;
    // Validate before touching the endpoint table, so a bad event fails the same with or without
    // subscribers.
    let body = minimal_body(event_id, event_type, transaction.tenant_id(), created_at, data)?;
    let endpoints: Vec<(String,)> = sqlx::query_as(
        "SELECT e.id::text FROM webhook_endpoints e JOIN host_systems h ON h.tenant_id = e.tenant_id AND h.system_id = e.system_id \
         WHERE e.tenant_id = $1::uuid AND e.system_id = $2 AND e.enabled AND h.enabled \
           AND (cardinality(e.event_types) = 0 OR $3 = ANY (e.event_types)) ORDER BY e.id",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(event_type)
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| EventError::Database)?;
    let mut queued = 0;
    for (endpoint,) in endpoints {
        let endpoint: Uuid = endpoint.parse().map_err(|_| EventError::Database)?;
        if enqueue_delivery(transaction, endpoint, event_id, event_type, &body, DEFAULT_MAX_ATTEMPTS).await?.is_some() {
            queued += 1;
        }
    }
    Ok(queued)
}

/// The hosts that have mapped `document` and the host's own id for it, at most
/// [`MAX_HOSTS_PER_DOCUMENT`] (ordered, so fan-out is deterministic).
pub async fn document_hosts(transaction: &mut TenantTransaction<'_>, document: Uuid) -> Result<Vec<(String, String)>, EventError> {
    sqlx::query_as(
        "SELECT system_id, external_id FROM external_mappings \
         WHERE tenant_id = $1::uuid AND resource_kind = 'document' AND internal_id = $2::uuid ORDER BY system_id, external_id LIMIT 20",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| EventError::Database)
}

#[cfg(test)]
mod tests {
    use figcraft_cloud_domain::{TenantId, Uuid};

    use super::{DATA_KEYS, minimal_body, subscribed};

    fn body(event_type: &str, data: &[(&str, &str)]) -> Result<serde_json::Value, super::EventError> {
        minimal_body(Uuid::new_v4(), event_type, TenantId::random(), 1_790_000_000, data).map(|bytes| serde_json::from_slice(&bytes).expect("json"))
    }

    #[test]
    fn bodies_carry_ids_and_nothing_else() {
        let value = body("document.created", &[("document_id", "d-1"), ("external_document_id", "ext 9"), ("workspace_id", "w-1")]).expect("body");
        assert_eq!(value["type"], "document.created");
        assert_eq!(value["created_at"], 1_790_000_000);
        let mut keys: Vec<&str> = value.as_object().expect("object").keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["created_at", "data", "id", "tenant_id", "type"]);
        assert_eq!(value["data"].as_object().map(serde_json::Map::len), Some(3));
        assert!(body("export.completed", &[]).is_ok());
    }

    #[test]
    fn anything_that_could_carry_content_is_refused() {
        for key in ["name", "document_name", "content", "email", "comment", "title", "url", "token", "secret", ""] {
            assert!(body("document.created", &[(key, "x")]).is_err(), "{key}");
        }
        assert!(body("document.created", &[("document_id", "")]).is_err());
        assert!(body("document.created", &[("document_id", "a\nb")]).is_err());
        assert!(body("document.created", &[("document_id", &"x".repeat(513))]).is_err());
        assert!(body("document.created", &[("document_id", "a"), ("document_id", "b")]).is_err());
        for event in ["document.exploded", "", "*", "ai.job.completed"] {
            assert!(body(event, &[]).is_err(), "{event}");
        }
        assert!(DATA_KEYS.iter().all(|key| !key.contains("name") && !key.contains("content") && !key.contains("email")));
    }

    #[test]
    fn subscriptions_filter_by_exact_type() {
        assert!(subscribed(&[], "document.created"));
        assert!(subscribed(&["document.created".into()], "document.created"));
        assert!(!subscribed(&["document.created".into()], "document.archived"));
        assert!(!subscribed(&["document.created".into()], "document.create"));
        assert!(subscribed(&["document.archived".into(), "document.created".into()], "document.created"));
    }
}
