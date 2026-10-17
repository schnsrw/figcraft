//! Document history reads (S6-040): who changed a document, when and through what; the checkpoints
//! the system keeps; and "last edited by".
//!
//! Three things people mix up are kept apart:
//!
//! * the **activity** is the event log read as a story: consecutive batches by the same person,
//!   through the same access, with no pause longer than [`ACTIVITY_GAP_SECS`], form one *run*
//!   ("Ada, through the Design team, 14:02 to 14:19, 37 changes");
//! * **checkpoints** are the verified snapshots the relay stores so a room can be opened quickly.
//!   They are an implementation detail made visible, not something a person made or named;
//! * **named versions** (`versions.rs`) are the ones people save on purpose.
//!
//! Reads are bounded ([`MAX_EVENTS_SCANNED`] events per page) and keyed by sequence, newest first.
//! Grouping is a pure function ([`group`]) so it is tested without a database.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Uuid};
use std::collections::BTreeMap;
use std::fmt;

use crate::TenantTransaction;
use crate::attribution::Via;
use crate::profiles::color_for;

/// A pause longer than this ends a run (five minutes).
pub const ACTIVITY_GAP_SECS: i64 = 300;
/// Most events one activity page scans.
pub const MAX_EVENTS_SCANNED: i64 = 2000;
/// Most runs one activity page returns.
pub const MAX_RUNS: usize = 100;
/// Most checkpoints one page returns.
pub const MAX_CHECKPOINTS: i64 = 100;

/// Fixed, non-leaking categories for history reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryError {
    Database,
    CorruptRow,
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "history error: {self:?}")
    }
}

impl std::error::Error for HistoryError {}

/// One committed batch, as much of it as history needs (never its content).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityEvent {
    pub seq: i64,
    pub principal_id: PrincipalId,
    pub display_name: String,
    pub color: String,
    pub via: Option<Via>,
    /// Unix seconds.
    pub at: i64,
}

/// Consecutive batches by one person through one access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityRun {
    pub from_seq: i64,
    pub to_seq: i64,
    pub batches: u32,
    pub started_at: i64,
    pub ended_at: i64,
    pub principal_id: PrincipalId,
    pub display_name: String,
    pub color: String,
    pub via: Option<Via>,
}

/// Group `events` (newest first, as read) into runs, newest first: a new run starts when the person
/// or the access differs, or the pause between two batches exceeds `gap_secs`.
#[must_use]
pub fn group(events: &[ActivityEvent], gap_secs: i64) -> Vec<ActivityRun> {
    let mut runs: Vec<ActivityRun> = Vec::new();
    for event in events {
        if let Some(run) = runs.last_mut()
            && run.principal_id == event.principal_id
            && run.via == event.via
            && run.started_at.saturating_sub(event.at) <= gap_secs
        {
            run.from_seq = event.seq;
            run.batches = run.batches.saturating_add(1);
            run.started_at = event.at.min(run.started_at);
            continue;
        }
        runs.push(ActivityRun {
            from_seq: event.seq,
            to_seq: event.seq,
            batches: 1,
            started_at: event.at,
            ended_at: event.at,
            principal_id: event.principal_id,
            display_name: event.display_name.clone(),
            color: event.color.clone(),
            via: event.via,
        });
    }
    runs
}

/// How a run reads: "Ada", or "Ada, through the Design team", or "Ada, through a share link
/// (Design review)". `via_name` is the team's name or the link's label, when known.
#[must_use]
pub fn describe(display_name: &str, via: Option<Via>, via_name: Option<&str>) -> String {
    match (via, via_name) {
        (Some(Via::Team(_)), Some(name)) => format!("{display_name}, through the {name} team"),
        (Some(Via::Team(_)), None) => format!("{display_name}, through a team"),
        (Some(Via::ShareLink(_)), Some(label)) => format!("{display_name}, through a share link ({label})"),
        (Some(Via::ShareLink(_)), None) => format!("{display_name}, through a share link"),
        (Some(Via::ServiceAccount(_)), Some(name)) => format!("{display_name}, through the {name} integration"),
        (Some(Via::ServiceAccount(_)), None) => format!("{display_name}, through an integration"),
        (Some(Via::Member) | None, _) => display_name.to_string(),
    }
}

type EventRow = (i64, String, String, Option<String>, Option<String>, Option<String>, i64);

fn event_from(row: EventRow) -> Result<ActivityEvent, HistoryError> {
    let (seq, principal, display_name, color, via_kind, via_id, at) = row;
    let principal_id: PrincipalId = principal.parse().map_err(|_| HistoryError::CorruptRow)?;
    let via_id = via_id.as_deref().map(Uuid::parse_str).transpose().map_err(|_| HistoryError::CorruptRow)?;
    Ok(ActivityEvent {
        seq,
        principal_id,
        color: color.unwrap_or_else(|| color_for(principal_id).to_string()),
        display_name,
        via: Via::from_parts(via_kind.as_deref(), via_id),
        at,
    })
}

/// A page of activity: runs newest first, and the sequence to pass as `before_seq` for the next page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityPage {
    pub runs: Vec<ActivityRun>,
    pub next_before_seq: Option<i64>,
}

/// Read activity older than `before_seq` (`None`: from the newest), at most `limit` runs (capped at
/// [`MAX_RUNS`]). A run cut off by the scan bound is left for the next page so a run is never split
/// across pages unless it alone exceeds the bound.
pub async fn read_activity(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    before_seq: Option<i64>,
    limit: usize,
) -> Result<ActivityPage, HistoryError> {
    let limit = limit.clamp(1, MAX_RUNS);
    let rows: Vec<EventRow> = sqlx::query_as(concat!(
        "SELECT e.seq, e.actor_principal_id::text, p.display_name, p.color, e.via_kind, e.via_id::text, ",
        "extract(epoch FROM e.created_at)::bigint FROM document_events e JOIN principals p ON p.id = e.actor_principal_id ",
        "WHERE e.tenant_id = $1::uuid AND e.document_id = $2::uuid AND ($3::bigint IS NULL OR e.seq < $3::bigint) ",
        "ORDER BY e.seq DESC LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(before_seq)
    .bind(MAX_EVENTS_SCANNED)
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| HistoryError::Database)?;
    let exhausted = i64::try_from(rows.len()).unwrap_or(i64::MAX) < MAX_EVENTS_SCANNED;
    let events = rows.into_iter().map(event_from).collect::<Result<Vec<_>, _>>()?;
    let mut runs = group(&events, ACTIVITY_GAP_SECS);
    // When the scan stopped before the start of the log, its last (oldest) run may continue in
    // events not read: leave it for the next page, unless it is all there is.
    if !exhausted && runs.len() > 1 {
        runs.pop();
    }
    let more = !exhausted || runs.len() > limit;
    runs.truncate(limit);
    let next_before_seq = if more { runs.last().map(|run| run.from_seq) } else { None };
    Ok(ActivityPage { runs, next_before_seq })
}

/// The names behind the `via` of some runs: team names and share-link labels, by id.
pub async fn via_names(transaction: &mut TenantTransaction<'_>, vias: &[Via]) -> Result<BTreeMap<Uuid, String>, HistoryError> {
    let teams: Vec<String> = vias.iter().filter_map(|via| matches!(via, Via::Team(_)).then(|| via.id().map(|id| id.to_string())).flatten()).collect();
    let links: Vec<String> =
        vias.iter().filter_map(|via| matches!(via, Via::ShareLink(_)).then(|| via.id().map(|id| id.to_string())).flatten()).collect();
    let mut names = BTreeMap::new();
    if !teams.is_empty() {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT id::text, name FROM teams WHERE tenant_id = $1::uuid AND id = ANY($2::uuid[])")
            .bind(transaction.tenant_id().to_string())
            .bind(&teams)
            .fetch_all(transaction.connection())
            .await
            .map_err(|_| HistoryError::Database)?;
        for (id, name) in rows {
            names.insert(Uuid::parse_str(&id).map_err(|_| HistoryError::CorruptRow)?, name);
        }
    }
    if !links.is_empty() {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id::text, label FROM share_links WHERE tenant_id = $1::uuid AND id = ANY($2::uuid[])")
                .bind(transaction.tenant_id().to_string())
                .bind(&links)
                .fetch_all(transaction.connection())
                .await
                .map_err(|_| HistoryError::Database)?;
        for (id, label) in rows {
            if let Some(label) = label {
                names.insert(Uuid::parse_str(&id).map_err(|_| HistoryError::CorruptRow)?, label);
            }
        }
    }
    Ok(names)
}

/// A checkpoint: a verified snapshot the system keeps (never named, never restored by people
/// directly; versions are the thing people restore).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub id: Uuid,
    pub seq: i64,
    pub created_at: i64,
    pub size_bytes: i64,
}

/// Verified checkpoints newest first, older than `before_seq`, at most `limit` (capped at
/// [`MAX_CHECKPOINTS`]) plus one to tell whether another page exists.
pub async fn list_checkpoints(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    before_seq: Option<i64>,
    limit: i64,
) -> Result<Vec<Checkpoint>, HistoryError> {
    let limit = limit.clamp(1, MAX_CHECKPOINTS);
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(concat!(
        "SELECT id::text, seq, extract(epoch FROM created_at)::bigint, size_bytes FROM document_snapshots ",
        "WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND state = 'verified' AND ($3::bigint IS NULL OR seq < $3::bigint) ",
        "ORDER BY seq DESC, created_at DESC LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(before_seq)
    .bind(limit + 1)
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| HistoryError::Database)?;
    rows.into_iter()
        .map(|(id, seq, created_at, size_bytes)| {
            Ok(Checkpoint { id: Uuid::parse_str(&id).map_err(|_| HistoryError::CorruptRow)?, seq, created_at, size_bytes })
        })
        .collect()
}

/// The latest committed batch of a document: "last edited by".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LastEdit {
    pub seq: i64,
    pub at: i64,
    pub principal_id: PrincipalId,
    pub display_name: String,
    pub color: String,
    pub via: Option<Via>,
}

pub async fn last_edit(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId) -> Result<Option<LastEdit>, HistoryError> {
    let row: Option<EventRow> = sqlx::query_as(concat!(
        "SELECT e.seq, e.actor_principal_id::text, p.display_name, p.color, e.via_kind, e.via_id::text, ",
        "extract(epoch FROM e.created_at)::bigint FROM document_events e JOIN principals p ON p.id = e.actor_principal_id ",
        "WHERE e.tenant_id = $1::uuid AND e.document_id = $2::uuid ORDER BY e.seq DESC LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| HistoryError::Database)?;
    row.map(event_from).transpose().map(|event| {
        event.map(|e| LastEdit { seq: e.seq, at: e.at, principal_id: e.principal_id, display_name: e.display_name, color: e.color, via: e.via })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: i64, who: PrincipalId, via: Option<Via>, at: i64) -> ActivityEvent {
        ActivityEvent { seq, principal_id: who, display_name: "x".into(), color: "#000000".into(), via, at }
    }

    #[test]
    fn consecutive_batches_by_one_person_and_access_form_one_run() {
        let (ada, ben) = (PrincipalId::random(), PrincipalId::random());
        let team = Via::Team(Uuid::new_v4());
        // Newest first, as read.
        let events = vec![
            event(7, ada, Some(team), 1000),
            event(6, ada, Some(team), 900),
            event(5, ben, Some(Via::Member), 850),
            event(4, ada, Some(team), 700),
            event(3, ada, Some(team), 100),
            event(2, ada, None, 90),
            event(1, ada, None, 80),
        ];
        let runs = group(&events, 300);
        let shape: Vec<(i64, i64, u32)> = runs.iter().map(|r| (r.from_seq, r.to_seq, r.batches)).collect();
        // ada/team 6-7, ben 5, ada/team 4 alone (the pause to 3 was 600 s), ada/team 3 alone, ada/unknown 1-2.
        assert_eq!(shape, [(6, 7, 2), (5, 5, 1), (4, 4, 1), (3, 3, 1), (1, 2, 2)]);
        assert_eq!((runs[0].started_at, runs[0].ended_at), (900, 1000));
        // The same person through a different access is a different run, even without a pause.
        assert_ne!(runs[3].via, runs[4].via);
    }

    #[test]
    fn a_pause_exactly_at_the_gap_stays_one_run_and_one_second_more_splits() {
        let ada = PrincipalId::random();
        assert_eq!(group(&[event(2, ada, None, 400), event(1, ada, None, 100)], 300).len(), 1);
        assert_eq!(group(&[event(2, ada, None, 401), event(1, ada, None, 100)], 300).len(), 2);
        assert!(group(&[], 300).is_empty());
    }

    #[test]
    fn runs_read_as_a_person_or_a_person_through_something() {
        let id = Uuid::new_v4();
        assert_eq!(describe("Ada", Some(Via::Member), None), "Ada");
        assert_eq!(describe("Ada", None, None), "Ada");
        assert_eq!(describe("Ada", Some(Via::Team(id)), Some("Design")), "Ada, through the Design team");
        assert_eq!(describe("Ada", Some(Via::Team(id)), None), "Ada, through a team");
        assert_eq!(describe("Ada", Some(Via::ShareLink(id)), Some("Review")), "Ada, through a share link (Review)");
        assert_eq!(describe("Ada", Some(Via::ShareLink(id)), None), "Ada, through a share link");
    }

    #[test]
    fn rows_without_a_stored_colour_get_the_stable_default() {
        let who = PrincipalId::random();
        let row: EventRow = (3, who.to_string(), "Ada".into(), None, Some("team".into()), Some(Uuid::new_v4().to_string()), 5);
        let event = event_from(row).expect("event");
        assert_eq!(event.color, color_for(who));
        assert!(matches!(event.via, Some(Via::Team(_))));
        // A damaged via reads as unknown, not as an error.
        let row: EventRow = (3, who.to_string(), "Ada".into(), Some("#112233".into()), Some("team".into()), None, 5);
        assert_eq!(event_from(row).expect("event").via, None);
    }
}
