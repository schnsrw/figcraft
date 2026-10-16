//! Durable document event encoding and checksum format.
//!
//! A `document_events` row stores one batch the collaboration server accepted. Its `payload` is
//! that batch's existing collaboration wire form, [`figcraft_collab::message::encode`] of an
//! [`Accepted`] (the wire-version byte, then postcard), so replay hands the relay the same value
//! the in-memory server journaled and broadcast, with no second document format. The filesystem
//! server's `Journal::Accepted` log entries carry the same `Accepted` value, which keeps the S2-021
//! migration a re-encode rather than a translation.
//!
//! Only accepted batches are events: they alone advance the document sequence. Client-id issue and
//! batch refusals are session state, not ordered document history.
//!
//! `checksum` is SHA-256 over a domain-separated frame:
//!
//! ```text
//! "figcraft.cloud.document-event.v1\0"
//! tenant_id (16 bytes) || document_id (16 bytes) || seq (i64, big-endian)
//! payload length (u64, big-endian) || payload
//! ```
//!
//! Binding the tenant, document and sequence means a payload copied to another row, document or
//! tenant fails verification even though its bytes are intact. The fencing token and actor are
//! provenance checked by the append transaction (S2-015), not part of the event's content.

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_collab::Accepted;
use figcraft_collab::message;
use sha2::{Digest, Sha256};

/// Largest stored payload, matching the `document_events.payload` `CHECK` constraint. It is
/// smaller than the collaboration protocol's message limit, so the relay must refuse a larger
/// batch before confirming it rather than discover the limit at commit.
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Length of [`event_checksum`], matching the `document_events.checksum` `CHECK` constraint.
pub const EVENT_CHECKSUM_BYTES: usize = 32;

const CHECKSUM_DOMAIN: &[u8] = b"figcraft.cloud.document-event.v1\0";

/// Fixed categories for event encoding and verification failures. Decoder details are not exposed,
/// because they can quote stored document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCodecError {
    /// The batch could not be encoded.
    Encode,
    /// The payload is empty or larger than [`MAX_EVENT_PAYLOAD_BYTES`].
    PayloadSize,
    /// The sequence number is not in `1..=i64::MAX`.
    Sequence,
    /// The stored checksum does not match the row's identity and payload.
    Checksum,
    /// The payload is not a valid encoded batch.
    Decode,
    /// The payload decodes, but names a different sequence number than its row.
    SequenceMismatch,
}

impl std::fmt::Display for EventCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "document event codec error: {self:?}")
    }
}

impl std::error::Error for EventCodecError {}

/// One accepted batch ready to insert as a `document_events` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedEvent {
    pub seq: i64,
    pub payload: Vec<u8>,
    pub checksum: [u8; EVENT_CHECKSUM_BYTES],
}

/// Encode `batch` for storage under `tenant_id` and `document_id`.
pub fn encode_event(tenant_id: TenantId, document_id: CloudDocumentId, batch: &Accepted) -> Result<EncodedEvent, EventCodecError> {
    let seq = stored_seq(batch.seq)?;
    let payload = message::encode(batch).map_err(|_| EventCodecError::Encode)?;
    check_payload_size(&payload)?;
    let checksum = event_checksum(tenant_id, document_id, seq, &payload);
    Ok(EncodedEvent { seq, payload, checksum })
}

/// Verify a stored row and decode its batch.
///
/// The checksum is checked before the payload is decoded, so corrupted or relocated bytes never
/// reach the postcard decoder.
pub fn decode_event(
    tenant_id: TenantId,
    document_id: CloudDocumentId,
    seq: i64,
    payload: &[u8],
    checksum: &[u8],
) -> Result<Accepted, EventCodecError> {
    if seq <= 0 {
        return Err(EventCodecError::Sequence);
    }
    check_payload_size(payload)?;
    if checksum != event_checksum(tenant_id, document_id, seq, payload).as_slice() {
        return Err(EventCodecError::Checksum);
    }
    let batch = message::decode::<Accepted>(payload, MAX_EVENT_PAYLOAD_BYTES).map_err(|_| EventCodecError::Decode)?;
    if i64::try_from(batch.seq).ok() != Some(seq) {
        return Err(EventCodecError::SequenceMismatch);
    }
    Ok(batch)
}

/// The checksum of an event row, in the frame described in the module documentation.
#[must_use]
pub fn event_checksum(tenant_id: TenantId, document_id: CloudDocumentId, seq: i64, payload: &[u8]) -> [u8; EVENT_CHECKSUM_BYTES] {
    let length = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    Sha256::new()
        .chain_update(CHECKSUM_DOMAIN)
        .chain_update(tenant_id.as_uuid().as_bytes())
        .chain_update(document_id.as_uuid().as_bytes())
        .chain_update(seq.to_be_bytes())
        .chain_update(length.to_be_bytes())
        .chain_update(payload)
        .finalize()
        .into()
}

fn stored_seq(seq: u64) -> Result<i64, EventCodecError> {
    i64::try_from(seq).ok().filter(|seq| *seq > 0).ok_or(EventCodecError::Sequence)
}

fn check_payload_size(payload: &[u8]) -> Result<(), EventCodecError> {
    if payload.is_empty() || payload.len() > MAX_EVENT_PAYLOAD_BYTES {
        return Err(EventCodecError::PayloadSize);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_collab::{Change, Json, NodeId, Prop};

    fn ids() -> (TenantId, CloudDocumentId) {
        ("10000000-0000-4000-8000-000000000001".parse().unwrap(), "20000000-0000-4000-8000-000000000002".parse().unwrap())
    }

    fn batch(seq: u64) -> Accepted {
        Accepted {
            seq,
            author: 3,
            batch_id: 7,
            changes: vec![
                Change::Set { id: NodeId((4 << 32) | 9), prop: Prop::Name, value: Json::of("Café ☕").unwrap() },
                Change::Delete { id: NodeId(5) },
            ],
        }
    }

    #[test]
    fn round_trips_an_accepted_batch_in_the_collaboration_wire_form() {
        let (tenant, document) = ids();
        let encoded = encode_event(tenant, document, &batch(42)).unwrap();
        assert_eq!(encoded.seq, 42);
        assert_eq!(encoded.payload, message::encode(&batch(42)).unwrap());
        assert_eq!(encoded.payload.first(), Some(&message::WIRE_VERSION));
        assert_eq!(decode_event(tenant, document, 42, &encoded.payload, &encoded.checksum), Ok(batch(42)));
    }

    #[test]
    fn checksum_format_is_pinned() {
        let (tenant, document) = ids();
        let mut frame = Vec::new();
        frame.extend_from_slice(b"figcraft.cloud.document-event.v1\0");
        frame.extend_from_slice(&[0x10, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        frame.extend_from_slice(&[0x20, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]);
        frame.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 9]);
        frame.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"abc");
        let expected: [u8; 32] = Sha256::digest(&frame).into();
        assert_eq!(event_checksum(tenant, document, 9, b"abc"), expected);
    }

    #[test]
    fn checksum_binds_tenant_document_sequence_and_payload() {
        let (tenant, document) = ids();
        let encoded = encode_event(tenant, document, &batch(5)).unwrap();
        let other_tenant: TenantId = "10000000-0000-4000-8000-000000000009".parse().unwrap();
        let other_document: CloudDocumentId = "20000000-0000-4000-8000-000000000009".parse().unwrap();
        let (payload, checksum) = (&encoded.payload, &encoded.checksum);
        assert_eq!(decode_event(other_tenant, document, 5, payload, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, other_document, 5, payload, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, document, 6, payload, checksum), Err(EventCodecError::Checksum));
        let mut flipped = payload.clone();
        if let Some(last) = flipped.last_mut() {
            *last ^= 1;
        }
        assert_eq!(decode_event(tenant, document, 5, &flipped, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, document, 5, payload, &checksum[..31]), Err(EventCodecError::Checksum));
    }

    #[test]
    fn rejects_sequences_the_table_cannot_hold() {
        let (tenant, document) = ids();
        assert_eq!(encode_event(tenant, document, &batch(0)), Err(EventCodecError::Sequence));
        assert_eq!(encode_event(tenant, document, &batch(u64::MAX)), Err(EventCodecError::Sequence));
        assert_eq!(encode_event(tenant, document, &batch(i64::MAX as u64)).map(|event| event.seq), Ok(i64::MAX));
        assert_eq!(decode_event(tenant, document, 0, b"x", &[0; 32]), Err(EventCodecError::Sequence));
        assert_eq!(decode_event(tenant, document, -1, b"x", &[0; 32]), Err(EventCodecError::Sequence));
    }

    #[test]
    fn rejects_payloads_outside_the_stored_size_range() {
        let (tenant, document) = ids();
        let huge = Accepted {
            changes: vec![Change::Set { id: NodeId(1), prop: Prop::Name, value: Json::of(&"x".repeat(MAX_EVENT_PAYLOAD_BYTES)).unwrap() }],
            ..batch(1)
        };
        assert_eq!(encode_event(tenant, document, &huge), Err(EventCodecError::PayloadSize));
        assert_eq!(decode_event(tenant, document, 1, &[], &event_checksum(tenant, document, 1, &[])), Err(EventCodecError::PayloadSize));
    }

    #[test]
    fn rejects_checksummed_garbage_and_a_mismatched_inner_sequence() {
        let (tenant, document) = ids();
        for junk in [&[0u8][..], &[message::WIRE_VERSION], &[message::WIRE_VERSION, 0xff, 0xff, 0xff], &[9, 1, 2, 3]] {
            assert_eq!(decode_event(tenant, document, 1, junk, &event_checksum(tenant, document, 1, junk)), Err(EventCodecError::Decode));
        }
        let payload = message::encode(&batch(8)).unwrap();
        assert_eq!(
            decode_event(tenant, document, 9, &payload, &event_checksum(tenant, document, 9, &payload)),
            Err(EventCodecError::SequenceMismatch)
        );
    }

    #[test]
    fn truncated_and_mutated_payloads_with_valid_checksums_never_decode_or_panic() {
        let (tenant, document) = ids();
        let payload = message::encode(&batch(3)).unwrap();
        for end in 1..payload.len() {
            let prefix = payload.get(..end).unwrap();
            assert!(decode_event(tenant, document, 3, prefix, &event_checksum(tenant, document, 3, prefix)).is_err());
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..2_000 {
            let mut mutated = payload.clone();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = usize::try_from(state % payload.len() as u64).unwrap();
            if let Some(byte) = mutated.get_mut(index) {
                *byte = byte.wrapping_add(u8::try_from(state >> 56).unwrap() | 1);
            }
            let _ = decode_event(tenant, document, 3, &mutated, &event_checksum(tenant, document, 3, &mutated));
        }
    }
}
