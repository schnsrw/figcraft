//! TS-009 / S5-018 step 1: what happens to a document image of 100 KiB to 40 MiB at each stage of
//! the relay today, with images travelling inline (base64 in a `Put` of the batch). No database
//! and no object store: the stages are the same pure functions the relay and the storage layer
//! call (`Server::receive`, `events::encode_event`, `Server::state` + the snapshot bounds,
//! `Server::join`'s welcome), so the numbers are exact for the encodings and the failures are the
//! real ones. `docs/saas/large-images.md` explains the results.
//!
//! Run `cargo test -p figcraft-cloud-storage --test large_image_limits -- --nocapture` for the
//! table. The tests assert the characterised behaviour, so a change to any limit shows up here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::document_snapshots::MAX_SNAPSHOT_BYTES;
use figcraft_cloud_storage::events::{EventCodecError, MAX_EVENT_PAYLOAD_BYTES, MAX_REPLAY_BYTES, encode_event};
use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use figcraft_collab::{CatchUp, Client, Journal, Limits, NoRecompute, Outgoing, Server};
use figcraft_doc::{Document, ImageData};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;
/// 100 KiB, 1, 5, 12, 20 and 40 MiB.
const SIZES: [usize; 6] = [100 * KIB, MIB, 5 * MIB, 12 * MIB, 20 * MIB, 40 * MIB];

fn tenant() -> TenantId {
    "00000000-0000-4000-8000-0000000000a1".parse().expect("tenant id")
}

fn document() -> CloudDocumentId {
    "00000000-0000-4000-8000-0000000000b1".parse().expect("document id")
}

/// Bytes the collaboration layer never decodes, different for every size so hashes differ.
fn image_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G'];
    v.extend((0..n.saturating_sub(4)).map(|i| (i.wrapping_mul(31).wrapping_add(n)) as u8));
    v
}

fn hello(name: &str) -> figcraft_collab::Hello {
    figcraft_collab::Hello {
        protocol: message::PROTOCOL,
        file: "f".into(),
        token: "tok".into(),
        name: name.into(),
        color: "#FF8800".into(),
        client_version: message::client_version(),
        resume: None,
    }
}

fn join(server: &mut Server) -> Client {
    let (_, out) = server.join(hello("Ada")).expect("join");
    let Some(Outgoing::To(_, welcome)) = out.into_iter().next() else { panic!("no welcome") };
    Client::join(welcome).expect("client")
}

/// The batch a client sends after pasting an image of `n` bytes.
fn paste(client: &mut Client, n: usize) -> ClientMsg {
    let mut doc = client.document().clone();
    doc.images.insert(format!("{n:064x}"), ImageData { bytes: Arc::new(image_bytes(n)), width: 4000, height: 3000 });
    client.edit(&doc).expect("edit").expect("a batch")
}

struct Row {
    size: usize,
    wire: usize,
    accepted: bool,
    event: Result<usize, EventCodecError>,
    snapshot: usize,
    welcome: usize,
}

fn measure(n: usize) -> Row {
    let mut server = Server::new(Document::new("Measured"), "tok", Limits::default());
    server.journaling(true);
    let mut a = join(&mut server);
    let msg = paste(&mut a, n);
    // The transport refuses a message over `max_message_bytes` before the server sees it.
    let wire = message::encode(&msg).expect("encode").len();
    let out = if wire <= Limits::default().max_message_bytes { server.receive(a.id(), msg) } else { Vec::new() };
    let accepted = out.iter().any(|o| matches!(o, Outgoing::All(ServerMsg::Changes(_))));
    // What the relay would commit: `append_events` encodes each accepted batch (a failure there
    // fails the commit and closes the room).
    let event = server
        .take_journal()
        .into_iter()
        .find_map(|j| match j {
            Journal::Accepted(batch) => Some(encode_event(tenant(), document(), &batch).map(|e| e.payload.len())),
            _ => None,
        })
        .unwrap_or(Err(EventCodecError::Encode));
    let snapshot = server.state().ok().and_then(|s| message::encode(&s).ok()).map_or(0, |b| b.len());
    // What a client that joins later is sent (one message).
    let welcome = server
        .join(hello("Bo"))
        .ok()
        .and_then(|(_, out)| out.into_iter().find_map(|o| if let Outgoing::To(_, w @ ServerMsg::Welcome { .. }) = o { Some(w) } else { None }))
        .and_then(|w| message::encode(&w).ok())
        .map_or(0, |b| b.len());
    Row { size: n, wire, accepted, event, snapshot, welcome }
}

fn mib(n: usize) -> String {
    format!("{:.1}", n as f64 / MIB as f64)
}

#[test]
fn inline_images_stage_by_stage() {
    println!("\n{:>9} {:>10} {:>9} {:>14} {:>13} {:>12}", "image MiB", "batch MiB", "accepted", "event MiB", "snapshot MiB", "welcome MiB");
    let mut rows = Vec::new();
    for n in SIZES {
        let r = measure(n);
        let event = match &r.event {
            Ok(b) => mib(*b),
            Err(e) => format!("{e:?}"),
        };
        println!("{:>9} {:>10} {:>9} {:>14} {:>13} {:>12}", mib(r.size), mib(r.wire), r.accepted, event, mib(r.snapshot), mib(r.welcome));
        rows.push(r);
    }
    for r in &rows {
        // base64 inflates the image by 4/3 around a JSON string inside postcard.
        assert!(r.wire > r.size * 4 / 3, "the batch is at least the base64 of the image");
        assert!(r.wire < r.size * 4 / 3 + 4 * KIB, "and not much more");
        // The collaboration server accepts every batch the transport lets through (64 MiB).
        assert!(r.accepted, "{} MiB image: the collaboration server accepts it", mib(r.size));
        // The durable event is limited to 16 MiB: a 12 MiB image (16 MiB of base64 plus framing)
        // and larger is accepted by the server but can never be stored.
        let storable = r.wire <= MAX_EVENT_PAYLOAD_BYTES;
        assert_eq!(r.event.is_ok(), storable, "{} MiB image: the event encodes iff the batch is within 16 MiB", mib(r.size));
        if !storable {
            assert_eq!(r.event, Err(EventCodecError::PayloadSize));
        }
        // A late joiner downloads the image in the welcome.
        assert!(r.welcome >= r.size * 4 / 3, "a late joiner downloads the whole image inline");
        // The stored state holds it twice: in the document and in the log of recent batches.
        assert!(r.snapshot >= 2 * (r.size * 4 / 3), "stored state: {} MiB", mib(r.snapshot));
    }
    assert!(rows.iter().find(|r| r.size == 5 * MIB).is_some_and(|r| r.event.is_ok()));
    assert!(rows.iter().find(|r| r.size == 12 * MIB).is_some_and(|r| r.event.is_err()));
    // The 128 MiB snapshot bound holds the 40 MiB image (twice) but would not hold a few of them.
    let bound = usize::try_from(MAX_SNAPSHOT_BYTES).unwrap_or(usize::MAX);
    assert!(rows.iter().find(|r| r.size == 40 * MIB).is_some_and(|r| r.snapshot <= bound));
    assert!(3 * rows.iter().map(|r| r.snapshot).max().unwrap_or(0) > bound);
}

/// Replay pages hold up to 32 MiB of payload, but always the next event: one 16 MiB event fits a
/// page by itself, and two of them fill one.
#[test]
fn replay_pages_hold_whole_events() {
    // Mirrors the SQL rule in `read_events`: events are taken while their running sum is within
    // the budget, and always the first.
    let pages = |sizes: &[usize]| {
        let (mut pages, mut running) = (0, 0usize);
        for s in sizes {
            if pages == 0 || running + s > MAX_REPLAY_BYTES {
                pages += 1;
                running = 0;
            }
            running += s;
        }
        pages
    };
    assert_eq!(pages(&[MAX_EVENT_PAYLOAD_BYTES; 2]), 1);
    assert_eq!(pages(&[MAX_EVENT_PAYLOAD_BYTES; 4]), 2);
}

/// A batch the server accepts but the event log cannot store: the relay closes the room, the client
/// reconnects, still holds the batch as pending and sends it again, for ever.
#[test]
fn an_unstorable_batch_is_retried_for_ever() {
    let limits = Limits::default();
    let mut server = Server::new(Document::new("Measured"), "tok", limits);
    server.journaling(true);
    let mut a = join(&mut server);
    let committed = server.state().expect("state");
    let first = paste(&mut a, 12 * MIB);
    let mut msg = Some(first);
    for round in 1..=3 {
        let batch = msg.take().expect("a batch to send");
        let out = server.receive(a.id(), batch.clone());
        assert!(out.iter().any(|o| matches!(o, Outgoing::All(ServerMsg::Changes(_)))), "round {round}: accepted by the collaboration server");
        let accepted = server.take_journal().into_iter().find_map(|j| if let Journal::Accepted(b) = j { Some(b) } else { None }).expect("journaled");
        // The relay cannot store it: commit fails, the room is dropped and nobody is told.
        assert_eq!(encode_event(tenant(), document(), &accepted).map(|_| ()), Err(EventCodecError::PayloadSize), "round {round}");
        // The room reopens from what was committed; the client reconnects as itself.
        server = Server::restore(committed.clone(), "tok", limits).expect("restore");
        server.journaling(true);
        a.disconnect();
        let figcraft_collab::ClientMsg::Hello(mut again) = a.reconnect(hello("Ada")) else { panic!("not a hello") };
        again.resume = Some(a.resume());
        let (_, out) = server.join(again).expect("rejoin");
        let Some(Outgoing::To(_, welcome)) = out.into_iter().next() else { panic!("no welcome") };
        let resend = a.receive(welcome, &NoRecompute).expect("receive");
        assert_eq!(a.pending(), 1, "round {round}: the client still holds the batch");
        msg = resend.into_iter().find(|m| matches!(m, ClientMsg::Changes { .. }));
        assert!(msg.is_some(), "round {round}: and sends it again");
    }
}

/// Over the transport's own 64 MiB message limit the message is not even decoded: the sender's
/// connection is closed and it reconnects with the batch still pending.
#[test]
fn over_the_message_limit_the_connection_is_closed() {
    let mut server = Server::new(Document::new("Measured"), "tok", Limits::default());
    let mut a = join(&mut server);
    let msg = paste(&mut a, 49 * MIB);
    let bytes = message::encode(&msg).expect("encode");
    assert!(bytes.len() > Limits::default().max_message_bytes, "{} bytes is over the 64 MiB message limit", bytes.len());
    assert!(message::decode::<ClientMsg>(&bytes, Limits::default().max_message_bytes).is_err());
    assert_eq!(a.pending(), 1, "the client holds it");
}

#[test]
fn the_welcome_snapshot_carries_the_image() {
    let mut server = Server::new(Document::new("Measured"), "tok", Limits::default());
    let mut a = join(&mut server);
    let msg = paste(&mut a, 5 * MIB);
    let _ = server.receive(a.id(), msg);
    let (_, out) = server.join(hello("Bo")).expect("join");
    let Some(Outgoing::To(_, ServerMsg::Welcome { catch_up: CatchUp::Snapshot(changes), .. })) = out.into_iter().next() else {
        panic!("no welcome snapshot")
    };
    let weight = figcraft_collab::server::weight(&changes);
    assert!(weight >= 5 * MIB * 4 / 3, "the snapshot's weight counts the base64 bytes: {weight}");
}
