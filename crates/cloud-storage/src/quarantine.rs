//! Document recovery quarantine (`docs/saas/architecture.md`, "Room recovery").
//!
//! A document whose durable history cannot be restored (no verified snapshot chain and no
//! replayable log) is quarantined for operator repair instead of being served: the relay refuses
//! to open it until an operator clears the quarantine. The state lives on the `documents` row
//! (`quarantined_at` and `quarantine_reason`, both set or both null).
//!
//! [`quarantine_document`] is idempotent and keeps the first reason, so the earliest diagnosis is
//! what the operator sees. It writes no audit event, because the relay quarantines without a
//! principal; the row itself is the record. [`clear_quarantine`] is operator work and records a
//! `document.quarantine_cleared` audit event naming the operator and the reason it cleared, in the
//! caller's transaction.

use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Uuid};

use crate::TenantTransaction;
use crate::audit::record_audit_event;

/// The audit action [`clear_quarantine`] records.
pub const QUARANTINE_CLEARED_ACTION: &str = "document.quarantine_cleared";

/// Why a document was quarantined, matching the `documents.quarantine_reason` constraint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum QuarantineReason {
    /// Snapshots existed but none re-verified, and the log alone does not restore the document.
    SnapshotChainUnverifiable,
    /// The committed log is missing events below the document's committed sequence.
    LogGap,
    /// A stored event failed verification (checksum, encoding or inner sequence).
    LogCorrupt,
    /// The log reads cleanly but does not replay to the committed sequence.
    ReplayFailed,
    /// An operator quarantined the document by hand.
    Operator,
}

impl QuarantineReason {
    pub const ALL: [Self; 5] = [Self::SnapshotChainUnverifiable, Self::LogGap, Self::LogCorrupt, Self::ReplayFailed, Self::Operator];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotChainUnverifiable => "snapshot_chain_unverifiable",
            Self::LogGap => "log_gap",
            Self::LogCorrupt => "log_corrupt",
            Self::ReplayFailed => "replay_failed",
            Self::Operator => "operator",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == value)
    }
}

/// Fixed, non-leaking categories for quarantine failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineError {
    /// No such document in the transaction's tenant.
    NotFound,
    /// A serialization failure or deadlock; the caller may retry.
    Conflict,
    Constraint,
    Database,
    /// The stored quarantine state is not one this build understands.
    CorruptRow,
    /// The audit event could not be written, so the clearing must roll back.
    Audit,
}

impl fmt::Display for QuarantineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "document quarantine error: {self:?}")
    }
}

impl std::error::Error for QuarantineError {}

fn database_error(error: sqlx::Error) -> QuarantineError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("40001" | "40P01") => QuarantineError::Conflict,
        Some("23503" | "23514") => QuarantineError::Constraint,
        _ => QuarantineError::Database,
    }
}

/// Whether a quarantine write changed anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineChange {
    Applied,
    /// The document was already quarantined ([`quarantine_document`]) or was not quarantined
    /// ([`clear_quarantine`]); nothing was written.
    AlreadyApplied,
}

/// The document's quarantine reason, `None` when it is servable. An unknown document is
/// `NotFound`.
pub async fn quarantine_reason(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
) -> Result<Option<QuarantineReason>, QuarantineError> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT quarantine_reason FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    match row {
        None => Err(QuarantineError::NotFound),
        Some((None,)) => Ok(None),
        Some((Some(reason),)) => QuarantineReason::parse(&reason).map(Some).ok_or(QuarantineError::CorruptRow),
    }
}

/// Whether the document is quarantined. An unknown document is `NotFound`.
pub async fn is_quarantined(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> Result<bool, QuarantineError> {
    Ok(quarantine_reason(transaction, document).await?.is_some())
}

/// Quarantine `document` for `reason`. Idempotent: an already quarantined document keeps its
/// first reason and is `AlreadyApplied`.
pub async fn quarantine_document(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    reason: QuarantineReason,
) -> Result<QuarantineChange, QuarantineError> {
    let updated = sqlx::query(concat!(
        "UPDATE documents SET quarantined_at = now(), quarantine_reason = $3, updated_at = now() ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND quarantined_at IS NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(reason.as_str())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    if updated == 1 {
        return Ok(QuarantineChange::Applied);
    }
    // Either already quarantined or unknown; `quarantine_reason` tells which.
    quarantine_reason(transaction, document).await?;
    Ok(QuarantineChange::AlreadyApplied)
}

/// Clear `document`'s quarantine on behalf of `operator` (operator support tooling only, after
/// the history has been repaired), recording a [`QUARANTINE_CLEARED_ACTION`] audit event with the
/// cleared reason. A document that is not quarantined is `AlreadyApplied` and writes nothing.
pub async fn clear_quarantine(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    operator: PrincipalId,
) -> Result<QuarantineChange, QuarantineError> {
    let cleared: Option<(String,)> = sqlx::query_as(concat!(
        "WITH previous AS (SELECT id, quarantine_reason FROM documents ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND quarantined_at IS NOT NULL FOR UPDATE) ",
        "UPDATE documents d SET quarantined_at = NULL, quarantine_reason = NULL, updated_at = now() FROM previous ",
        "WHERE d.tenant_id = $1::uuid AND d.id = previous.id RETURNING previous.quarantine_reason"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((reason,)) = cleared else {
        quarantine_reason(transaction, document).await?;
        return Ok(QuarantineChange::AlreadyApplied);
    };
    let reason = QuarantineReason::parse(&reason).ok_or(QuarantineError::CorruptRow)?;
    let metadata = serde_json::json!({ "reason": reason.as_str() });
    record_audit_event(
        transaction,
        Uuid::new_v4(),
        Some(operator.as_uuid()),
        QUARANTINE_CLEARED_ACTION,
        "document",
        Some(document.as_uuid()),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await
    .map_err(|_| QuarantineError::Audit)?;
    Ok(QuarantineChange::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip_and_junk_does_not_parse() {
        for reason in QuarantineReason::ALL {
            assert_eq!(QuarantineReason::parse(reason.as_str()), Some(reason));
        }
        for junk in ["", " ", "LOG_GAP", "log_gap ", "log-gap", "operator\0", "\u{fffd}", &"x".repeat(4096)] {
            assert_eq!(QuarantineReason::parse(junk), None, "{junk:?}");
        }
    }
}
