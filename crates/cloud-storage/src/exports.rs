//! Export results and document thumbnails.
//!
//! # Exports
//!
//! [`request_export`] records what a principal asked for (document, format, bounded options and a
//! sequence no later than the document's `current_seq`) as a `queued` `exports` row and a durable
//! `jobs` row of kind [`EXPORT_JOB_KIND`] whose input names the export; the export points at the
//! job through `job_id`. A worker then moves the export through
//! `queued → running → succeeded | failed` with [`start_export`], [`complete_export`] and
//! [`fail_export`], mirroring each step onto the job row. The table's constraints back the state
//! machine: only a `succeeded` export has a result object, and only `failed` carries an error code.
//!
//! [`complete_export`] checks the bytes against the export's format, stores them under
//! `tenants/<tenant>/exports/<export>/<filename>` (the filename is reduced by
//! [`sanitize_filename`] to the object-key alphabet), registers and verifies the object by reading
//! it back, and adds an `export` reference so the result outlives nothing but its export.
//! [`expire_exports`] turns old `succeeded` exports into `expired` in bounded batches and drops
//! their references, after which the garbage collector (S3-015) may collect the bytes.
//!
//! What completion checks per format:
//!
//! - `png`, `jpeg`, `webp`, `figcraft`: the full [`crate::validation`] header and container checks
//!   (magic bytes, dimensions and pixel budgets; for `.figcraft`, the zip directory budgets and a
//!   `document.json` member).
//! - `svg`: UTF-8 text whose root element (after an optional BOM, XML declaration and comments) is
//!   `<svg`, ending in `</svg>` or a self-closed root, with no `<script`, `<foreignObject`,
//!   `<iframe`, `<embed`, `<object`, DOCTYPE or ENTITY declaration, `javascript:`/`vbscript:` URL
//!   or `on…=` event-handler attribute (ASCII case-insensitive). This is a tripwire for FigCraft's
//!   own renderer output, not a sanitizer: an attribute *value* that merely contains ` onx=` is
//!   refused too, and the result must still be served as an attachment (`docs/saas/security.md`).
//! - `pdf`: a `%PDF-n.n` header, `%%EOF` within the last 1 KiB, and no `/JavaScript`, `/JS`,
//!   `/Launch`, `/SubmitForm` or `/ImportData` name token in the uncompressed bytes. Names inside
//!   compressed object streams or written with `#xx` escapes are not seen; like SVG, this checks
//!   that worker output looks like what FigCraft writes rather than proving a PDF safe.
//!
//! # Thumbnails
//!
//! [`store_thumbnail`] stores one WebP per document sequence at the fixed key
//! `tenants/<tenant>/thumbnails/<document>/<seq>.webp`, validated against the small
//! [`THUMBNAIL_BUDGETS`], with a `document_thumbnails` row and a `thumbnail` reference owned by the
//! document. Objects are immutable, so a sequence keeps its first thumbnail: storing identical
//! bytes again is an idempotent no-op, different bytes for a stored sequence are
//! `ThumbnailExists`, and a sequence older than the document's newest thumbnail is `Superseded`.
//! Storing a newer sequence replaces the older thumbnails: their rows and references are dropped
//! (at most [`MAX_THUMBNAIL_PRUNE`] per call) so the collector can reclaim them.
//! [`latest_thumbnail`] returns the newest one.
//!
//! Callers must have authorized the principal for the document (S4); the functions here check
//! tenant membership, tenancy and state, never document roles.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::job_authorization::{JobAuthorization, JobAuthorizationError, JobPermission, recheck_before_commit};
use crate::object_metadata::{
    NewObject, ObjectKind, ObjectRecord, ObjectRepositoryError, Verification, get_object, keys, register_object, verify_object,
};
use crate::object_references::{Change, ObjectReferenceError, OwnerKind, add_reference, remove_reference};
use crate::objects::{ObjectStore, ObjectStoreError, valid_key};
use crate::uploads::valid_producer_version;
use crate::validation::{self, Budgets, MEDIA_FIGCRAFT, MEDIA_JPEG, MEDIA_PNG, MEDIA_WEBP, Rejection};

/// The `jobs.kind` of an export job.
pub const EXPORT_JOB_KIND: &str = "document.export";
/// Largest export result stored.
pub const MAX_EXPORT_BYTES: usize = 128 * 1024 * 1024;
/// Largest stored thumbnail.
pub const MAX_THUMBNAIL_BYTES: usize = 1024 * 1024;
/// Largest compact JSON encoding of export options. PostgreSQL's `jsonb` text form adds at most one
/// space per separator, so this stays under the table's 16 KiB bound.
pub const MAX_OPTIONS_BYTES: usize = 8 * 1024;
/// Deepest nesting of export options.
pub const MAX_OPTIONS_DEPTH: usize = 16;
/// Most JSON values (objects, arrays, strings, numbers and so on) in export options.
pub const MAX_OPTIONS_NODES: usize = 1024;
/// Most `queued` or `running` exports one principal may hold in one tenant.
pub const MAX_OPEN_EXPORTS_PER_PRINCIPAL: i64 = 32;
/// Most exports one [`expire_exports`] call transitions.
pub const MAX_EXPIRE_BATCH: i64 = 500;
/// The floor on how long a succeeded export is kept before it may expire, longer than any
/// download URL ([`crate::objects::MAX_PRESIGN_TTL`]).
pub const MIN_EXPORT_RETENTION: Duration = Duration::from_secs(60 * 60);
/// The cap on a requested retention (ten years), so the bound stays a small integer interval.
pub const MAX_EXPORT_RETENTION: Duration = Duration::from_secs(10 * 366 * 24 * 60 * 60);
/// Longest error code recorded on a failed export (the table's bound).
pub const MAX_ERROR_CODE_BYTES: usize = 128;
/// Longest sanitized filename stem, before the format's extension.
pub const MAX_FILENAME_STEM_BYTES: usize = 96;
/// Most older thumbnails one [`store_thumbnail`] call drops.
pub const MAX_THUMBNAIL_PRUNE: i64 = 16;
/// Budgets for thumbnails: small images only.
pub const THUMBNAIL_BUDGETS: Budgets = Budgets {
    max_input_bytes: MAX_THUMBNAIL_BYTES as u64,
    max_pixels: 1024 * 1024,
    max_dimension: 1024,
    max_decoded_bytes: 4 * 1024 * 1024,
    ..Budgets::DEFAULT
};
/// Budgets for raster and `.figcraft` export results.
pub const EXPORT_BUDGETS: Budgets = Budgets { max_input_bytes: MAX_EXPORT_BYTES as u64, ..Budgets::DEFAULT };

/// SVG media type.
pub const MEDIA_SVG: &str = "image/svg+xml";
/// PDF media type.
pub const MEDIA_PDF: &str = "application/pdf";

/// An export format, matching the `exports.format` constraint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExportFormat {
    Png,
    Jpeg,
    Webp,
    Svg,
    Pdf,
    Figcraft,
}

impl ExportFormat {
    pub const ALL: [Self; 6] = [Self::Png, Self::Jpeg, Self::Webp, Self::Svg, Self::Pdf, Self::Figcraft];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Svg => "svg",
            Self::Pdf => "pdf",
            Self::Figcraft => "figcraft",
        }
    }

    /// The format named exactly `value` (lowercase).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.as_str() == value)
    }

    /// The media type the result object is stored with.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => MEDIA_PNG,
            Self::Jpeg => MEDIA_JPEG,
            Self::Webp => MEDIA_WEBP,
            Self::Svg => MEDIA_SVG,
            Self::Pdf => MEDIA_PDF,
            Self::Figcraft => MEDIA_FIGCRAFT,
        }
    }

    /// The filename extension every stored result ends with.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Svg => "svg",
            Self::Pdf => "pdf",
            Self::Figcraft => "figcraft",
        }
    }

    /// Extensions a caller-supplied filename may already end with (stripped before the canonical
    /// one is appended, so `frame.PNG` becomes `frame.png`, not `frame.png.png`).
    const fn known_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Png => &[".png"],
            Self::Jpeg => &[".jpg", ".jpeg"],
            Self::Webp => &[".webp"],
            Self::Svg => &[".svg"],
            Self::Pdf => &[".pdf"],
            Self::Figcraft => &[".figcraft"],
        }
    }
}

/// An export's lifecycle state, matching the `exports.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Expired,
}

impl ExportState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// Fixed, non-leaking categories for export and thumbnail failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportError {
    /// No such export or document in the transaction's tenant.
    NotFound,
    /// The principal is not an active member of the transaction's tenant.
    NotAuthorized,
    /// The document is not `active` or `archived`.
    DocumentUnavailable,
    /// The requested sequence is past the document's `current_seq`.
    SeqAhead,
    /// The sequence does not fit the table's `BIGINT`.
    InvalidSeq,
    /// The options are not a JSON object within the size, depth and node bounds, or hold a NUL.
    InvalidOptions,
    /// The principal already holds [`MAX_OPEN_EXPORTS_PER_PRINCIPAL`] open exports.
    TooManyOpenExports,
    /// The export's (or its job's) state does not permit the transition.
    InvalidState,
    /// The error code is not 1–128 bytes of lowercase letters, digits, `.`, `_` and `-`.
    InvalidErrorCode,
    /// The producer version is empty, too long or not printable ASCII.
    InvalidProducerVersion,
    /// The bytes are empty or larger than the kind's maximum.
    InvalidSize,
    /// The bytes failed the format's header or container checks.
    InvalidContent(Rejection),
    /// SVG or PDF bytes carry scripting, external content or entity declarations.
    ActiveContent,
    /// The document already has a thumbnail for a newer sequence.
    Superseded,
    /// A different thumbnail is already stored for this sequence.
    ThumbnailExists,
    /// The object key cannot be built from the inputs.
    InvalidKey,
    /// An object the operation depends on is not in a usable state.
    ObjectConflict,
    /// The stored bytes' read-back did not match what was written.
    VerificationFailed,
    /// The requester lost `document.export` on the document since the export was requested
    /// (S4-013): roll back, then [`fail_export`] with
    /// [`AUTHORIZATION_REVOKED`](crate::job_authorization::AUTHORIZATION_REVOKED).
    AuthorizationRevoked,
    /// A unique violation or a serialization failure; the caller may retry.
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The object store failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "export error: {self:?}")
    }
}

impl std::error::Error for ExportError {}

fn database_error(error: sqlx::Error) -> ExportError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => ExportError::Conflict,
        Some("23503" | "23514") => ExportError::Constraint,
        _ => ExportError::Database,
    }
}

fn object_error(error: ObjectRepositoryError) -> ExportError {
    match error {
        ObjectRepositoryError::Conflict => ExportError::Conflict,
        ObjectRepositoryError::Constraint => ExportError::Constraint,
        ObjectRepositoryError::Database => ExportError::Database,
        ObjectRepositoryError::CorruptRow => ExportError::CorruptRow,
        ObjectRepositoryError::InvalidState => ExportError::ObjectConflict,
        ObjectRepositoryError::Store(error) => ExportError::Store(error),
    }
}

fn reference_error(error: ObjectReferenceError) -> ExportError {
    match error {
        ObjectReferenceError::NotFound | ObjectReferenceError::InvalidState => ExportError::ObjectConflict,
        ObjectReferenceError::Conflict => ExportError::Conflict,
        ObjectReferenceError::Constraint => ExportError::Constraint,
        ObjectReferenceError::CorruptRow => ExportError::CorruptRow,
        ObjectReferenceError::Referenced
        | ObjectReferenceError::TooYoung
        | ObjectReferenceError::TooManyHolds
        | ObjectReferenceError::InvalidLimit
        | ObjectReferenceError::Database => ExportError::Database,
    }
}

// ---- filenames and keys ----

/// Reduce a caller-supplied filename to a safe key segment ending in `format`'s extension.
///
/// Only the last `/`- or `\`-separated component is kept. ASCII letters are lowercased, digits,
/// `.`, `_` and `-` are kept, and every other character (spaces, Unicode, controls, `:` …) becomes
/// `-`; runs of `-` or `.` collapse to one. A known extension for the format is stripped, leading
/// and trailing `.`, `_` and `-` are trimmed, and the stem is cut to [`MAX_FILENAME_STEM_BYTES`].
/// An empty stem becomes `export`. The result is never `.` or `..`, never starts with `.`, and is
/// at most `MAX_FILENAME_STEM_BYTES + 10` bytes. Work is linear in the input.
#[must_use]
pub fn sanitize_filename(name: &str, format: ExportFormat) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    // Scan a little past the stem bound so a trailing extension can still be recognized.
    let scan_bytes = MAX_FILENAME_STEM_BYTES + 16;
    let mut mapped = String::with_capacity(scan_bytes);
    for character in base.chars() {
        if mapped.len() >= scan_bytes {
            break;
        }
        let next = match character {
            'a'..='z' | '0'..='9' | '_' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            '.' => '.',
            _ => '-',
        };
        if matches!(next, '-' | '.') && mapped.ends_with(next) {
            continue;
        }
        mapped.push(next);
    }
    let mut stem = mapped.as_str();
    for extension in format.known_extensions() {
        if let Some(stripped) = stem.strip_suffix(extension) {
            stem = stripped;
            break;
        }
    }
    let trim = |value: &str| value.trim_matches(|character| matches!(character, '.' | '-' | '_')).to_string();
    let mut stem = trim(stem);
    if stem.len() > MAX_FILENAME_STEM_BYTES {
        // Every character is ASCII here, so any byte index is a character boundary.
        stem = trim(stem.get(..MAX_FILENAME_STEM_BYTES).unwrap_or_default());
    }
    if stem.is_empty() {
        stem = "export".to_string();
    }
    format!("{stem}.{}", format.extension())
}

/// The key of export `export`'s result named `filename` (already sanitized), or `None` when the
/// segment is not a single valid key segment.
#[must_use]
pub fn export_key(tenant: TenantId, export: Uuid, filename: &str) -> Option<String> {
    if filename.is_empty() || filename.contains('/') || filename.starts_with('.') {
        return None;
    }
    let key = format!("tenants/{tenant}/exports/{export}/{filename}");
    valid_key(&key).then_some(key)
}

// ---- content checks ----

/// Whether `haystack` contains `needle`, ignoring ASCII case.
fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

/// Whether `text` has an `on…=` attribute: whitespace or `/`, then `on`, one or more ASCII
/// letters, optional whitespace and `=`. Linear: each letter run is walked once.
fn has_event_handler(text: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < text.len() {
        let boundary = text.get(index).is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'/');
        let on = text.get(index + 1..index + 3).is_some_and(|pair| pair.eq_ignore_ascii_case(b"on"));
        if !(boundary && on) {
            index += 1;
            continue;
        }
        let mut cursor = index + 3;
        let letters_start = cursor;
        while text.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
            cursor += 1;
        }
        let letters = cursor > letters_start;
        while text.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if letters && text.get(cursor) == Some(&b'=') {
            return true;
        }
        index = cursor.max(index + 1);
    }
    false
}

/// Most leading comments skipped before the SVG root element.
const MAX_SVG_PROLOG_COMMENTS: usize = 16;

/// The SVG checks documented on the module.
fn check_svg(bytes: &[u8]) -> Result<(), ExportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ExportError::InvalidContent(Rejection::Malformed))?;
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text).trim_start();
    if let Some(declaration) = rest.strip_prefix("<?xml") {
        let end = declaration.find("?>").ok_or(ExportError::InvalidContent(Rejection::Truncated))?;
        rest = declaration.get(end + 2..).unwrap_or_default().trim_start();
    }
    for _ in 0..MAX_SVG_PROLOG_COMMENTS {
        let Some(comment) = rest.strip_prefix("<!--") else {
            break;
        };
        let end = comment.find("-->").ok_or(ExportError::InvalidContent(Rejection::Truncated))?;
        rest = comment.get(end + 3..).unwrap_or_default().trim_start();
    }
    let root = rest.strip_prefix("<svg").ok_or(ExportError::InvalidContent(Rejection::MediaMismatch))?;
    if !root.starts_with(|character: char| character.is_ascii_whitespace() || matches!(character, '>' | '/')) {
        return Err(ExportError::InvalidContent(Rejection::MediaMismatch));
    }
    // The document ends with the root's end tag, or is a single self-closed root element.
    let trimmed = rest.trim_end();
    let self_closed = trimmed.find('>').is_some_and(|close| close + 1 == trimmed.len() && trimmed.get(..close).is_some_and(|tag| tag.ends_with('/')));
    if !(trimmed.ends_with("</svg>") || self_closed) {
        return Err(ExportError::InvalidContent(Rejection::Truncated));
    }
    const FORBIDDEN: [&[u8]; 9] =
        [b"<script", b"<foreignobject", b"<iframe", b"<embed", b"<object", b"<!doctype", b"<!entity", b"javascript:", b"vbscript:"];
    if FORBIDDEN.iter().any(|needle| contains_ignore_case(bytes, needle)) || has_event_handler(bytes) {
        return Err(ExportError::ActiveContent);
    }
    Ok(())
}

/// Bytes at the end of a PDF searched for `%%EOF`.
const PDF_TAIL_BYTES: usize = 1024;

/// Whether a PDF name token `name` (including `/`) appears followed by a delimiter or the end.
fn has_pdf_name(bytes: &[u8], name: &[u8]) -> bool {
    let mut start = 0;
    while let Some(offset) = bytes.get(start..).and_then(|rest| rest.windows(name.len()).position(|window| window == name)) {
        let end = start + offset + name.len();
        match bytes.get(end) {
            None => return true,
            Some(byte) if byte.is_ascii_whitespace() || b"/<>[](){}%".contains(byte) => return true,
            Some(_) => start = start + offset + 1,
        }
    }
    false
}

/// The PDF checks documented on the module.
fn check_pdf(bytes: &[u8]) -> Result<(), ExportError> {
    let header = bytes.get(..8).ok_or(ExportError::InvalidContent(Rejection::Truncated))?;
    let version_ok = header.get(5).is_some_and(u8::is_ascii_digit) && header.get(6) == Some(&b'.') && header.get(7).is_some_and(u8::is_ascii_digit);
    if !header.starts_with(b"%PDF-") || !version_ok {
        return Err(ExportError::InvalidContent(Rejection::MediaMismatch));
    }
    let tail = bytes.get(bytes.len().saturating_sub(PDF_TAIL_BYTES)..).unwrap_or_default();
    if !tail.windows(5).any(|window| window == b"%%EOF") {
        return Err(ExportError::InvalidContent(Rejection::Truncated));
    }
    const FORBIDDEN: [&[u8]; 5] = [b"/JavaScript", b"/JS", b"/Launch", b"/SubmitForm", b"/ImportData"];
    if FORBIDDEN.iter().any(|name| has_pdf_name(bytes, name)) {
        return Err(ExportError::ActiveContent);
    }
    Ok(())
}

/// Check export result bytes against `format`, as documented on the module.
pub fn check_export_content(format: ExportFormat, bytes: &[u8]) -> Result<(), ExportError> {
    if bytes.is_empty() || bytes.len() > MAX_EXPORT_BYTES {
        return Err(ExportError::InvalidSize);
    }
    match format {
        ExportFormat::Png | ExportFormat::Jpeg | ExportFormat::Webp | ExportFormat::Figcraft => {
            validation::validate(format.media_type(), bytes, &EXPORT_BUDGETS).map(|_| ()).map_err(ExportError::InvalidContent)
        }
        ExportFormat::Svg => check_svg(bytes),
        ExportFormat::Pdf => check_pdf(bytes),
    }
}

/// Check thumbnail bytes: a WebP within [`THUMBNAIL_BUDGETS`].
pub fn check_thumbnail_content(bytes: &[u8]) -> Result<(), ExportError> {
    if bytes.is_empty() || bytes.len() > MAX_THUMBNAIL_BYTES {
        return Err(ExportError::InvalidSize);
    }
    validation::validate(MEDIA_WEBP, bytes, &THUMBNAIL_BUDGETS).map(|_| ()).map_err(ExportError::InvalidContent)
}

// ---- options and codes ----

/// Check export options and return their compact JSON encoding.
///
/// The value must be an object of at most [`MAX_OPTIONS_DEPTH`] levels and [`MAX_OPTIONS_NODES`]
/// values, with no NUL in any key or string (PostgreSQL `jsonb` refuses `\u0000`), encoding to at
/// most [`MAX_OPTIONS_BYTES`]. The structure is walked iteratively before it is encoded, so a
/// deeply nested caller-built value cannot exhaust the stack.
pub fn encode_options(options: &serde_json::Value) -> Result<String, ExportError> {
    if !options.is_object() {
        return Err(ExportError::InvalidOptions);
    }
    let mut stack = vec![(options, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_OPTIONS_NODES || depth > MAX_OPTIONS_DEPTH {
            return Err(ExportError::InvalidOptions);
        }
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key.contains('\0') {
                        return Err(ExportError::InvalidOptions);
                    }
                    stack.push((child, depth + 1));
                }
            }
            serde_json::Value::Array(items) => stack.extend(items.iter().map(|child| (child, depth + 1))),
            serde_json::Value::String(text) if text.contains('\0') => return Err(ExportError::InvalidOptions),
            _ => {}
        }
        if stack.len() > MAX_OPTIONS_NODES {
            return Err(ExportError::InvalidOptions);
        }
    }
    let encoded = serde_json::to_string(options).map_err(|_| ExportError::InvalidOptions)?;
    if encoded.len() > MAX_OPTIONS_BYTES {
        return Err(ExportError::InvalidOptions);
    }
    Ok(encoded)
}

/// A failure code: 1–128 bytes of lowercase letters, digits, `.`, `_` and `-`, so it is a fixed
/// category rather than a message that could carry document content.
#[must_use]
pub fn valid_error_code(code: &str) -> bool {
    (1..=MAX_ERROR_CODE_BYTES).contains(&code.len())
        && code.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-'))
}

// ---- export rows ----

/// An `exports` row.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportRecord {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub document_id: CloudDocumentId,
    pub requested_by: PrincipalId,
    pub requested_seq: u64,
    pub format: ExportFormat,
    pub options: serde_json::Value,
    pub job_id: Option<Uuid>,
    pub result_object_id: Option<Uuid>,
    pub state: ExportState,
    pub error_code: Option<String>,
    /// Completion as whole Unix seconds (rounded down).
    pub completed_at_unix: Option<i64>,
}

type ExportRow = (String, String, String, String, i64, String, String, Option<String>, Option<String>, String, Option<String>, Option<i64>);

/// The selected columns, as a literal so every query stays a compile-time string.
macro_rules! columns {
    () => {
        concat!(
            "id::text, tenant_id::text, document_id::text, requested_by::text, requested_seq, format, options::text, job_id::text, ",
            "result_object_id::text, state, error_code, floor(extract(epoch FROM completed_at))::bigint"
        )
    };
}

fn parse_uuid(value: Option<String>) -> Result<Option<Uuid>, ExportError> {
    value.map(|id| id.parse()).transpose().map_err(|_| ExportError::CorruptRow)
}

fn export_record(row: ExportRow) -> Result<ExportRecord, ExportError> {
    let (id, tenant_id, document_id, requested_by, requested_seq, format, options, job_id, result_object_id, state, error_code, completed_at) = row;
    Ok(ExportRecord {
        id: id.parse().map_err(|_| ExportError::CorruptRow)?,
        tenant_id: tenant_id.parse().map_err(|_| ExportError::CorruptRow)?,
        document_id: document_id.parse().map_err(|_| ExportError::CorruptRow)?,
        requested_by: requested_by.parse().map_err(|_| ExportError::CorruptRow)?,
        requested_seq: u64::try_from(requested_seq).map_err(|_| ExportError::CorruptRow)?,
        format: ExportFormat::parse(&format).ok_or(ExportError::CorruptRow)?,
        // Bounded to 16 KiB by the table; serde_json's recursion limit bounds the nesting.
        options: serde_json::from_str(&options).map_err(|_| ExportError::CorruptRow)?,
        job_id: parse_uuid(job_id)?,
        result_object_id: parse_uuid(result_object_id)?,
        state: ExportState::parse(&state).ok_or(ExportError::CorruptRow)?,
        error_code,
        completed_at_unix: completed_at,
    })
}

/// Lock the document against deletion (`FOR KEY SHARE`, which does not block event appends) and
/// return its `current_seq`, refusing documents that are not `active` or `archived`.
async fn lock_document(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> Result<i64, ExportError> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT current_seq, state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR KEY SHARE")
            .bind(transaction.tenant_id().to_string())
            .bind(document.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    let (current_seq, state) = row.ok_or(ExportError::NotFound)?;
    if !matches!(state.as_str(), "active" | "archived") {
        return Err(ExportError::DocumentUnavailable);
    }
    Ok(current_seq)
}

/// Request an export of `document` at `requested_seq` in `format` for `principal`.
///
/// Inserts a `queued` export and its `queued` job in one transaction. The principal must be an
/// active member of the transaction's tenant (`NotAuthorized`); an unknown or foreign document is
/// `NotFound`; a sequence past `current_seq` is `SeqAhead`. Callers must have authorized the
/// principal to export the document, and should wrap the call in an idempotency key (S2).
pub async fn request_export(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    principal: PrincipalId,
    format: ExportFormat,
    options: &serde_json::Value,
    requested_seq: u64,
) -> Result<ExportRecord, ExportError> {
    let options = encode_options(options)?;
    let requested_seq = i64::try_from(requested_seq).map_err(|_| ExportError::InvalidSeq)?;
    let member: Option<i32> = sqlx::query_scalar(concat!(
        "SELECT 1 FROM tenant_members member JOIN principals principal ON principal.id = member.principal_id ",
        "WHERE member.tenant_id = $1::uuid AND member.principal_id = $2::uuid AND member.state = 'active' AND principal.state = 'active'"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if member.is_none() {
        return Err(ExportError::NotAuthorized);
    }
    let current_seq = lock_document(transaction, document).await?;
    if requested_seq > current_seq {
        return Err(ExportError::SeqAhead);
    }
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM exports WHERE tenant_id = $1::uuid AND requested_by = $2::uuid AND state IN ('queued', 'running')")
            .bind(transaction.tenant_id().to_string())
            .bind(principal.to_string())
            .fetch_one(transaction.connection())
            .await
            .map_err(database_error)?;
    if open >= MAX_OPEN_EXPORTS_PER_PRINCIPAL {
        return Err(ExportError::TooManyOpenExports);
    }

    let export_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    let input = serde_json::json!({
        "export_id": export_id.to_string(),
        "document_id": document.to_string(),
        "format": format.as_str(),
        "requested_seq": requested_seq,
    });
    let input = serde_json::to_string(&input).map_err(|_| ExportError::InvalidOptions)?;
    sqlx::query("INSERT INTO jobs (id, tenant_id, kind, input) VALUES ($1::uuid, $2::uuid, $3, $4::jsonb)")
        .bind(job_id.to_string())
        .bind(transaction.tenant_id().to_string())
        .bind(EXPORT_JOB_KIND)
        .bind(input)
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    let row: ExportRow = sqlx::query_as(concat!(
        "INSERT INTO exports (id, tenant_id, document_id, requested_by, requested_seq, format, options, job_id) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5, $6, $7::jsonb, $8::uuid) RETURNING ",
        columns!()
    ))
    .bind(export_id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(principal.to_string())
    .bind(requested_seq)
    .bind(format.as_str())
    .bind(options)
    .bind(job_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    export_record(row)
}

/// Read one export in the transaction's tenant.
pub async fn get_export(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<ExportRecord>, ExportError> {
    let row: Option<ExportRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM exports WHERE tenant_id = $1::uuid AND id = $2::uuid"))
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(export_record).transpose()
}

async fn lock_export(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<ExportRecord, ExportError> {
    let row: Option<ExportRow> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM exports WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(export_record).transpose()?.ok_or(ExportError::NotFound)
}

/// Move a locked export from one of `from` to `to`, setting the result, code and completion time
/// the table requires for `to`.
async fn transition(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    to: &str,
    result_object_id: Option<Uuid>,
    error_code: Option<&str>,
) -> Result<ExportRecord, ExportError> {
    let row: Option<ExportRow> = sqlx::query_as(concat!(
        "UPDATE exports SET state = $3, result_object_id = $4::uuid, error_code = $5, ",
        "completed_at = CASE WHEN $3 IN ('succeeded', 'failed') THEN now() END ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('queued', 'running') ",
        "AND ($3 <> 'running' OR state = 'queued') AND ($3 <> 'succeeded' OR state = 'running') RETURNING ",
        columns!()
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(to)
    .bind(result_object_id.map(|id| id.to_string()))
    .bind(error_code)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(export_record).transpose()?.ok_or(ExportError::InvalidState)
}

/// Mirror an export transition onto its job. A job that is no longer queued or running (cancelled
/// by an operator, say) refuses the transition with `InvalidState`.
async fn update_job(
    transaction: &mut TenantTransaction<'_>,
    job: Option<Uuid>,
    state: &str,
    result: Option<&serde_json::Value>,
    error_code: Option<&str>,
) -> Result<(), ExportError> {
    let Some(job) = job else {
        return Ok(());
    };
    let result = result.map(serde_json::to_string).transpose().map_err(|_| ExportError::Database)?;
    let updated = sqlx::query(concat!(
        "UPDATE jobs SET state = $3, result = $4::jsonb, error_code = $5, updated_at = now(), ",
        "attempts = attempts + CASE WHEN $3 = 'running' AND state = 'queued' THEN 1 ELSE 0 END ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND kind = 'document.export' AND state IN ('queued', 'running')"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(job.to_string())
    .bind(state)
    .bind(result)
    .bind(error_code)
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    if updated == 0 {
        return Err(ExportError::InvalidState);
    }
    Ok(())
}

/// Start a `queued` export (the worker picked it up). Starting a `running` export again returns it
/// unchanged, so a worker that lost its lease can resume; any other state is `InvalidState`.
pub async fn start_export(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<ExportRecord, ExportError> {
    let export = lock_export(transaction, id).await?;
    match export.state {
        ExportState::Running => Ok(export),
        ExportState::Queued => {
            update_job(transaction, export.job_id, "running", None, None).await?;
            transition(transaction, id, "running", None, None).await
        }
        ExportState::Succeeded | ExportState::Failed | ExportState::Expired => Err(ExportError::InvalidState),
    }
}

/// Fail a `queued` or `running` export with a fixed `error_code` ([`valid_error_code`]).
pub async fn fail_export(transaction: &mut TenantTransaction<'_>, id: Uuid, error_code: &str) -> Result<ExportRecord, ExportError> {
    if !valid_error_code(error_code) {
        return Err(ExportError::InvalidErrorCode);
    }
    let export = lock_export(transaction, id).await?;
    if !matches!(export.state, ExportState::Queued | ExportState::Running) {
        return Err(ExportError::InvalidState);
    }
    update_job(transaction, export.job_id, "failed", None, Some(error_code)).await?;
    transition(transaction, id, "failed", None, Some(error_code)).await
}

/// A completed export and its verified result object.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletedExport {
    pub export: ExportRecord,
    pub object: ObjectRecord,
}

/// Complete a `running` export with its result `bytes`.
///
/// The bytes are checked against the export's format ([`check_export_content`]); a refusal changes
/// nothing, so the worker can [`fail_export`] with the code. The filename is sanitized
/// ([`sanitize_filename`]) into the key `tenants/<tenant>/exports/<export>/<filename>`. The bytes
/// are written, registered as an `export` object, verified by reading them back, referenced by the
/// export, and the export becomes `succeeded` with its job. Only a `running` export completes;
/// completing it twice is `InvalidState`. Before anything is written the requester's access is
/// re-checked ([`recheck_before_commit`], S4-013): a requester who lost `document.export` since
/// the request gets `AuthorizationRevoked`, and the worker rolls back and calls [`fail_export`]
/// with `authorization_revoked`. On any `Err` roll back: bytes already written under the
/// export's own key are overwritten by a retry or reclaimed with the export prefix.
pub async fn complete_export(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    id: Uuid,
    bytes: Vec<u8>,
    filename: &str,
    producer_version: &str,
) -> Result<CompletedExport, ExportError> {
    if !valid_producer_version(producer_version) {
        return Err(ExportError::InvalidProducerVersion);
    }
    let export = lock_export(transaction, id).await?;
    if export.state != ExportState::Running {
        return Err(ExportError::InvalidState);
    }
    // S4-013: the requester must still be allowed to export the document now, at commit time.
    let job = JobAuthorization { requested_by: export.requested_by, document: export.document_id, permission: JobPermission::DocumentExport };
    recheck_before_commit(transaction, &job).await.map_err(|error| match error {
        JobAuthorizationError::Revoked => ExportError::AuthorizationRevoked,
        JobAuthorizationError::Database => ExportError::Database,
    })?;
    check_export_content(export.format, &bytes)?;
    let filename = sanitize_filename(filename, export.format);
    let object_key = export_key(transaction.tenant_id(), export.id, &filename).ok_or(ExportError::InvalidKey)?;
    let media_type = export.format.media_type();
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Export,
        object_key: object_key.clone(),
        sha256: Sha256::digest(&bytes).into(),
        size_bytes: i64::try_from(bytes.len()).map_err(|_| ExportError::InvalidSize)?,
        media_type: media_type.to_string(),
        producer_version: producer_version.to_string(),
    };
    let object_id = register_object(transaction, store.bucket_name(), &object).await.map_err(object_error)?.id;
    store.put_object(&object_key, bytes, media_type).await.map_err(ExportError::Store)?;
    let object = match verify_object(transaction, store, object_id).await.map_err(object_error)? {
        Some(Verification::Verified(object)) => object,
        Some(Verification::Rejected(..)) => return Err(ExportError::VerificationFailed),
        None => return Err(ExportError::CorruptRow),
    };
    add_reference(transaction, object.id, OwnerKind::Export, export.id).await.map_err(reference_error)?;
    let result = serde_json::json!({ "export_id": export.id.to_string(), "object_id": object.id.to_string() });
    update_job(transaction, export.job_id, "succeeded", Some(&result), None).await?;
    let export = transition(transaction, id, "succeeded", Some(object.id), None).await?;
    Ok(CompletedExport { export, object })
}

/// An export [`expire_exports`] retired, and the result object it no longer references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredExport {
    pub export_id: Uuid,
    pub object_id: Uuid,
}

/// Expire up to `limit` (capped to [`MAX_EXPIRE_BATCH`]) of the tenant's `succeeded` exports
/// completed more than `older_than` ago (never less than [`MIN_EXPORT_RETENTION`]), oldest first.
///
/// Each becomes `expired`, loses its result object and its `export` reference in the same
/// statement, so the object becomes collectable once nothing else references it. Rows another
/// transaction holds are skipped. Call again while a full batch comes back.
pub async fn expire_exports(transaction: &mut TenantTransaction<'_>, older_than: Duration, limit: i64) -> Result<Vec<ExpiredExport>, ExportError> {
    let limit = limit.clamp(0, MAX_EXPIRE_BATCH);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let age = older_than.clamp(MIN_EXPORT_RETENTION, MAX_EXPORT_RETENTION);
    let age_seconds = i64::try_from(age.as_secs()).map_err(|_| ExportError::Database)?;
    let rows: Vec<(String, String)> = sqlx::query_as(concat!(
        "WITH due AS (SELECT id, result_object_id FROM exports WHERE tenant_id = $1::uuid AND state = 'succeeded' ",
        "AND completed_at < now() - $2::bigint * interval '1 second' ORDER BY completed_at, id LIMIT $3 FOR UPDATE SKIP LOCKED), ",
        "dropped AS (DELETE FROM object_references reference USING due WHERE reference.tenant_id = $1::uuid ",
        "AND reference.object_id = due.result_object_id AND reference.owner_kind = 'export' AND reference.owner_id = due.id) ",
        "UPDATE exports AS export SET state = 'expired', result_object_id = NULL FROM due ",
        "WHERE export.tenant_id = $1::uuid AND export.id = due.id RETURNING export.id::text, due.result_object_id::text"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(age_seconds)
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter()
        .map(|(export_id, object_id)| {
            Ok(ExpiredExport {
                export_id: export_id.parse().map_err(|_| ExportError::CorruptRow)?,
                object_id: object_id.parse().map_err(|_| ExportError::CorruptRow)?,
            })
        })
        .collect()
}

// ---- thumbnails ----

/// A `document_thumbnails` row and its object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thumbnail {
    pub document_id: CloudDocumentId,
    pub seq: u64,
    pub object: ObjectRecord,
}

/// The result of [`store_thumbnail`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredThumbnail {
    pub thumbnail: Thumbnail,
    /// `AlreadyApplied` when identical bytes were already stored for this sequence and nothing was
    /// written.
    pub change: Change,
    /// Objects of older thumbnails whose rows and references this call dropped.
    pub superseded: Vec<Uuid>,
}

/// Store `bytes` as the thumbnail of `document` at `seq` (see the module documentation).
///
/// The sequence must not be past the document's `current_seq` (`SeqAhead`). Two concurrent stores
/// of different sequences can both commit; the older row is then dropped by the next store, and
/// [`latest_thumbnail`] returns the newer one meanwhile. On any `Err` roll back.
pub async fn store_thumbnail(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    document: CloudDocumentId,
    seq: u64,
    bytes: Vec<u8>,
    producer_version: &str,
) -> Result<StoredThumbnail, ExportError> {
    if !valid_producer_version(producer_version) {
        return Err(ExportError::InvalidProducerVersion);
    }
    let seq_value = i64::try_from(seq).map_err(|_| ExportError::InvalidSeq)?;
    check_thumbnail_content(&bytes)?;
    let current_seq = lock_document(transaction, document).await?;
    if seq_value > current_seq {
        return Err(ExportError::SeqAhead);
    }
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    let size_bytes = i64::try_from(bytes.len()).map_err(|_| ExportError::InvalidSize)?;

    let newest: Option<(i64, String)> = sqlx::query_as(concat!(
        "SELECT seq, object_id::text FROM document_thumbnails WHERE tenant_id = $1::uuid AND document_id = $2::uuid ",
        "ORDER BY seq DESC LIMIT 1 FOR UPDATE"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if let Some((newest_seq, object_id)) = newest {
        if newest_seq > seq_value {
            return Err(ExportError::Superseded);
        }
        if newest_seq == seq_value {
            let object_id: Uuid = object_id.parse().map_err(|_| ExportError::CorruptRow)?;
            let object = get_object(transaction, object_id).await.map_err(object_error)?.ok_or(ExportError::CorruptRow)?;
            if object.sha256 != sha256 || object.size_bytes != size_bytes {
                return Err(ExportError::ThumbnailExists);
            }
            add_reference(transaction, object.id, OwnerKind::Thumbnail, document.as_uuid()).await.map_err(reference_error)?;
            let thumbnail = Thumbnail { document_id: document, seq, object };
            return Ok(StoredThumbnail { thumbnail, change: Change::AlreadyApplied, superseded: Vec::new() });
        }
    }

    let object_key = keys::thumbnail(transaction.tenant_id(), document, seq);
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Thumbnail,
        object_key: object_key.clone(),
        sha256,
        size_bytes,
        media_type: MEDIA_WEBP.to_string(),
        producer_version: producer_version.to_string(),
    };
    // A live object still holding this sequence's key (an older thumbnail row was removed but the
    // collector has not retired its object yet) is a unique violation: `Conflict`.
    let object_id = register_object(transaction, store.bucket_name(), &object).await.map_err(object_error)?.id;
    store.put_object(&object_key, bytes, MEDIA_WEBP).await.map_err(ExportError::Store)?;
    let object = match verify_object(transaction, store, object_id).await.map_err(object_error)? {
        Some(Verification::Verified(object)) => object,
        Some(Verification::Rejected(..)) => return Err(ExportError::VerificationFailed),
        None => return Err(ExportError::CorruptRow),
    };
    sqlx::query("INSERT INTO document_thumbnails (tenant_id, document_id, seq, object_id) VALUES ($1::uuid, $2::uuid, $3, $4::uuid)")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .bind(seq_value)
        .bind(object.id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    add_reference(transaction, object.id, OwnerKind::Thumbnail, document.as_uuid()).await.map_err(reference_error)?;

    let pruned: Vec<String> = sqlx::query_scalar(concat!(
        "WITH stale AS (SELECT seq FROM document_thumbnails WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq < $3 ",
        "ORDER BY seq LIMIT $4 FOR UPDATE SKIP LOCKED) ",
        "DELETE FROM document_thumbnails AS thumbnail USING stale WHERE thumbnail.tenant_id = $1::uuid ",
        "AND thumbnail.document_id = $2::uuid AND thumbnail.seq = stale.seq RETURNING thumbnail.object_id::text"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(seq_value)
    .bind(MAX_THUMBNAIL_PRUNE)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    let mut superseded = Vec::with_capacity(pruned.len());
    for old in pruned {
        let old: Uuid = old.parse().map_err(|_| ExportError::CorruptRow)?;
        remove_reference(transaction, old, OwnerKind::Thumbnail, document.as_uuid()).await.map_err(reference_error)?;
        superseded.push(old);
    }
    Ok(StoredThumbnail { thumbnail: Thumbnail { document_id: document, seq, object }, change: Change::Applied, superseded })
}

/// The newest thumbnail of `document` in the transaction's tenant, if any.
pub async fn latest_thumbnail(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> Result<Option<Thumbnail>, ExportError> {
    let row: Option<(i64, String)> = sqlx::query_as(concat!(
        "SELECT seq, object_id::text FROM document_thumbnails WHERE tenant_id = $1::uuid AND document_id = $2::uuid ",
        "ORDER BY seq DESC LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((seq, object_id)) = row else {
        return Ok(None);
    };
    let object_id: Uuid = object_id.parse().map_err(|_| ExportError::CorruptRow)?;
    let object = get_object(transaction, object_id).await.map_err(object_error)?.ok_or(ExportError::CorruptRow)?;
    Ok(Some(Thumbnail { document_id: document, seq: u64::try_from(seq).map_err(|_| ExportError::CorruptRow)?, object }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        "10000000-0000-4000-8000-000000000001".parse().unwrap()
    }

    fn junk_names() -> Vec<String> {
        let mut junk: Vec<String> = [
            "",
            ".",
            "..",
            "...",
            "/",
            "//",
            "\\",
            "../../etc/passwd",
            "..\\..\\windows\\system32",
            "a/../../b",
            "C:\\Users\\me\\Frame 1.PNG",
            ".hidden",
            "-dash",
            "_under",
            "name.",
            "name..png",
            "Résumé — final.png",
            "日本語.png",
            "emoji 🎨.svg",
            "\0",
            "a\0b",
            "line\nbreak",
            "tab\tname",
            "%2e%2e%2f",
            "con",
            "a:b*c?d\"e<f>g|h",
            " ",
            "   .png",
            ".png",
            ".PNG",
            "x.png.png",
            "x.jpeg",
            "x.figcraft",
        ]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
        junk.push("a".repeat(100_000));
        junk.push("/".repeat(10_000));
        junk.push(".".repeat(10_000));
        junk.push("é".repeat(10_000));
        junk.push(format!("{}x.png", "-".repeat(500)));
        junk.extend((0u8..=255).map(|byte| String::from_utf8_lossy(&[b'f', byte, b'.', b'p', b'n', b'g']).into_owned()));
        junk.extend((0u32..0x3000).step_by(7).filter_map(char::from_u32).map(|character| format!("{character}{character}.svg")));
        junk
    }

    #[test]
    fn sanitized_filenames_are_single_safe_key_segments() {
        let export = Uuid::new_v4();
        for name in junk_names() {
            for format in ExportFormat::ALL {
                let sanitized = sanitize_filename(&name, format);
                assert!(sanitized.len() <= MAX_FILENAME_STEM_BYTES + 10, "{name:?} -> {sanitized:?}");
                assert!(sanitized.ends_with(&format!(".{}", format.extension())), "{sanitized}");
                assert!(!sanitized.starts_with('.') && !sanitized.starts_with('-') && !sanitized.starts_with('_'), "{sanitized}");
                assert!(!sanitized.contains('/') && !sanitized.contains("..") && !sanitized.contains("--"), "{sanitized}");
                assert!(sanitized.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)), "{sanitized}");
                let key = export_key(tenant(), export, &sanitized).unwrap_or_else(|| panic!("{name:?} -> {sanitized:?}"));
                assert!(valid_key(&key));
                assert_eq!(key.matches('/').count(), 4, "{key}");
            }
        }
    }

    #[test]
    fn filenames_keep_their_meaning_where_they_can() {
        assert_eq!(sanitize_filename("Frame 1.PNG", ExportFormat::Png), "frame-1.png");
        assert_eq!(sanitize_filename("C:\\Users\\me\\Frame 1.png", ExportFormat::Png), "frame-1.png");
        assert_eq!(sanitize_filename("../../etc/passwd", ExportFormat::Pdf), "passwd.pdf");
        assert_eq!(sanitize_filename("photo.jpeg", ExportFormat::Jpeg), "photo.jpg");
        assert_eq!(sanitize_filename("x.png", ExportFormat::Svg), "x.png.svg");
        assert_eq!(sanitize_filename("", ExportFormat::Figcraft), "export.figcraft");
        assert_eq!(sanitize_filename("..", ExportFormat::Webp), "export.webp");
        assert_eq!(sanitize_filename("日本語", ExportFormat::Png), "export.png");
        assert_eq!(sanitize_filename(&"a".repeat(1000), ExportFormat::Png).len(), MAX_FILENAME_STEM_BYTES + 4);
    }

    #[test]
    fn export_keys_refuse_anything_but_one_segment() {
        let export = Uuid::new_v4();
        for bad in ["", ".", "..", ".png", "a/b.png", "A.png", "a b.png", "a\0.png", "é.png"] {
            assert_eq!(export_key(tenant(), export, bad), None, "{bad:?}");
        }
        assert_eq!(export_key(tenant(), export, "a.png"), Some(format!("tenants/{}/exports/{export}/a.png", tenant())));
    }

    #[test]
    fn formats_round_trip_and_junk_formats_and_states_are_refused() {
        for format in ExportFormat::ALL {
            assert_eq!(ExportFormat::parse(format.as_str()), Some(format));
        }
        for name in junk_names().iter().chain(["PNG", "jpg", "png ", "gif", "html"].map(String::from).iter()) {
            assert_eq!(ExportFormat::parse(name), None, "{name:?}");
            assert_eq!(ExportState::parse(name), None);
        }
    }

    #[test]
    fn error_codes_are_fixed_categories() {
        for code in ["render_failed", "font.missing", "timeout-1", &"a".repeat(128)] {
            assert!(valid_error_code(code), "{code}");
        }
        for code in junk_names().iter().chain(["Render", "a b", &"a".repeat(129)].map(String::from).iter()) {
            if code.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte))
                && (1..=128).contains(&code.len())
            {
                continue;
            }
            assert!(!valid_error_code(code), "{code:?}");
        }
    }

    #[test]
    fn options_are_bounded_objects() {
        assert_eq!(encode_options(&serde_json::json!({})), Ok("{}".to_string()));
        assert!(encode_options(&serde_json::json!({ "scale": 2, "nodes": ["1:2"], "background": null })).is_ok());
        for bad in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!("x"),
            serde_json::json!(1),
            serde_json::json!({ "a": "x\u{0}y" }),
            serde_json::json!({ "a\u{0}": 1 }),
            serde_json::json!({ "a": "x".repeat(MAX_OPTIONS_BYTES) }),
            serde_json::json!({ "a": vec![0; MAX_OPTIONS_NODES] }),
        ] {
            assert_eq!(encode_options(&bad), Err(ExportError::InvalidOptions), "{bad}");
        }
        // Nesting far past serde's own limits is refused without recursing.
        let mut deep = serde_json::json!({});
        for _ in 0..100_000 {
            deep = serde_json::Value::Object(std::iter::once(("a".to_string(), deep)).collect());
        }
        assert_eq!(encode_options(&deep), Err(ExportError::InvalidOptions));
        // Drop iteratively too: serde_json's Value drop is recursive.
        let mut value = deep;
        while let serde_json::Value::Object(mut map) = value {
            value = map.remove("a").unwrap_or(serde_json::Value::Null);
        }
        let mut shallow = serde_json::json!({});
        for _ in 0..MAX_OPTIONS_DEPTH - 1 {
            shallow = serde_json::Value::Object(std::iter::once(("a".to_string(), shallow)).collect());
        }
        assert!(encode_options(&shallow).is_ok());
        assert_eq!(encode_options(&serde_json::json!({ "a": shallow })), Err(ExportError::InvalidOptions));
    }

    const SVG: &str = r##"<svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10" fill="#ABCDEF"/></svg>
"##;

    #[test]
    fn svg_checks_accept_figcraft_output_and_refuse_active_content() {
        assert_eq!(check_export_content(ExportFormat::Svg, SVG.as_bytes()), Ok(()));
        let prolog = format!("\u{feff}<?xml version=\"1.0\"?>\n<!-- made by FigCraft -->\n{SVG}");
        assert_eq!(check_export_content(ExportFormat::Svg, prolog.as_bytes()), Ok(()));
        assert_eq!(check_export_content(ExportFormat::Svg, b"<svg/>"), Ok(()));
        for active in [
            "<svg><script>alert(1)</script></svg>",
            "<svg><SCRIPT/></svg>",
            "<svg onload=\"x\"></svg>",
            "<svg\nONLOAD = \"x\"></svg>",
            "<svg><a href=\"javascript:x\"/></svg>",
            "<svg><a href=\"JavaScript:x\"/></svg>",
            "<svg><foreignObject/></svg>",
            "<svg><iframe/></svg>",
            "<!DOCTYPE svg [<!ENTITY a \"b\">]><svg/>",
            "<svg><!ENTITY a \"b\"></svg>",
            "<svg><g/onclick=\"x\"/></svg>",
        ] {
            assert!(check_export_content(ExportFormat::Svg, active.as_bytes()).is_err(), "{active}");
        }
        for (bad, expected) in [
            ("<svg", Rejection::MediaMismatch),
            ("<svgx/>", Rejection::MediaMismatch),
            ("<html/>", Rejection::MediaMismatch),
            ("<svg><rect/>", Rejection::Truncated),
            ("<?xml version=\"1.0\"", Rejection::Truncated),
            ("<!-- open", Rejection::Truncated),
        ] {
            assert_eq!(check_export_content(ExportFormat::Svg, bad.as_bytes()), Err(ExportError::InvalidContent(expected)), "{bad}");
        }
        assert_eq!(check_export_content(ExportFormat::Svg, b"<svg>\xff</svg>"), Err(ExportError::InvalidContent(Rejection::Malformed)));
        // Attributes that only look like handlers are fine.
        assert_eq!(check_export_content(ExportFormat::Svg, b"<svg data-only=\"1\"><g id=\"ont\"/></svg>"), Ok(()));
    }

    #[test]
    fn pdf_checks_need_a_header_an_end_and_no_actions() {
        let pdf = b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n";
        assert_eq!(check_export_content(ExportFormat::Pdf, pdf), Ok(()));
        // `/JSON` is not `/JS`.
        assert_eq!(check_export_content(ExportFormat::Pdf, b"%PDF-1.7\n<< /JSON 1 >>\n%%EOF"), Ok(()));
        for active in [&b"%PDF-1.7\n<< /S /JavaScript /JS (x) >>\n%%EOF"[..], b"%PDF-1.7\n<</JS(x)>>%%EOF", b"%PDF-2.0 /Launch\n%%EOF"] {
            assert_eq!(check_export_content(ExportFormat::Pdf, active), Err(ExportError::ActiveContent));
        }
        assert_eq!(check_export_content(ExportFormat::Pdf, b"%PDF-x.7\n%%EOF"), Err(ExportError::InvalidContent(Rejection::MediaMismatch)));
        assert_eq!(check_export_content(ExportFormat::Pdf, b"%PDF-1.7\nno end"), Err(ExportError::InvalidContent(Rejection::Truncated)));
        let mut late = pdf.to_vec();
        late.extend(vec![b' '; 2000]);
        assert_eq!(check_export_content(ExportFormat::Pdf, &late), Err(ExportError::InvalidContent(Rejection::Truncated)));
    }

    #[test]
    fn junk_content_is_refused_for_every_format_without_panicking() {
        let mut junk: Vec<Vec<u8>> =
            vec![vec![], vec![0], b"%PDF".to_vec(), b"<svg".to_vec(), b"\x89PNG\r\n\x1a\n".to_vec(), b"RIFF\0\0\0\0WEBP".to_vec()];
        junk.extend((0u8..=255).map(|byte| vec![byte; 64]));
        junk.extend((0..64).map(|length| b"<svg on".iter().copied().cycle().take(length).collect()));
        junk.extend((0..64).map(|length| b"%PDF-1.7 /JS".iter().copied().cycle().take(length).collect()));
        for bytes in &junk {
            for format in ExportFormat::ALL {
                assert!(check_export_content(format, bytes).is_err(), "{format:?} {bytes:?}");
            }
            assert!(check_thumbnail_content(bytes).is_err());
        }
        assert_eq!(check_export_content(ExportFormat::Png, &vec![0; MAX_EXPORT_BYTES + 1]), Err(ExportError::InvalidSize));
        assert_eq!(check_thumbnail_content(&vec![0; MAX_THUMBNAIL_BYTES + 1]), Err(ExportError::InvalidSize));
    }

    #[test]
    fn event_handler_scan_is_exact_at_the_edges() {
        for text in [&b" onx="[..], b"/onload =", b"\ton\x41b\t="] {
            assert!(has_event_handler(text), "{text:?}");
        }
        for text in [&b" on="[..], b"xonload=", b" on", b" onload", b" onload x=", b""] {
            assert!(!has_event_handler(text), "{text:?}");
        }
    }
}
