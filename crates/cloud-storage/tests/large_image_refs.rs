//! TS-009 / S5-018 step 4: the same stages as `large_image_limits.rs`, with images by reference.
//! A batch, its stored event, the stored state and a late joiner's welcome no longer depend on the
//! image's size (100 KiB to 100 MiB here), and a room that restarts (snapshot, event replay)
//! keeps the references. No database: the relay's room is the collaboration `Server` plus the
//! event encoding, which are the functions used here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::events::{MAX_EVENT_PAYLOAD_BYTES, decode_event, encode_event};
use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use figcraft_collab::{AssetDirectory, AssetPolicy, Client, Journal, Limits, Outgoing, Server, SessionState, assets};
use figcraft_doc::{Document, ImageData};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;
const SIZES: [usize; 8] = [100 * KIB, MIB, 5 * MIB, 12 * MIB, 20 * MIB, 40 * MIB, 64 * MIB, 100 * MIB];

fn tenant() -> TenantId {
    "00000000-0000-4000-8000-0000000000a1".parse().expect("tenant id")
}

fn document() -> CloudDocumentId {
    "00000000-0000-4000-8000-0000000000b1".parse().expect("document id")
}

/// The tenant's verified assets, as the relay's directory reports them.
#[derive(Debug, Default)]
struct Directory(Mutex<BTreeMap<String, u64>>);

impl AssetDirectory for Directory {
    fn verified(&self, hash: &str, len: u64) -> bool {
        self.0.lock().unwrap().get(hash).is_some_and(|l| *l == len)
    }
}

fn hello(words: &str) -> figcraft_collab::Hello {
    figcraft_collab::Hello {
        protocol: message::PROTOCOL,
        file: "f".into(),
        token: "tok".into(),
        name: "Ada".into(),
        color: "#FF8800".into(),
        client_version: words.into(),
        resume: None,
    }
}

fn room(directory: &Arc<Directory>) -> Server {
    let mut server = Server::new(Document::new("Measured"), "tok", Limits::default());
    server.set_assets(directory.clone(), AssetPolicy::default());
    server.set_max_batch_bytes(Some(15 * MIB));
    server.journaling(true);
    server
}

fn join(server: &mut Server, words: &str) -> Client {
    let (_, out) = server.join(hello(words)).expect("join");
    let Some(Outgoing::To(_, welcome)) = out.into_iter().next() else { panic!("no welcome") };
    Client::join(welcome).expect("client")
}

fn key(n: usize) -> String {
    format!("{n:064x}")
}

/// Pasting an image of `n` bytes the way the editor does once it has uploaded it.
fn paste_by_reference(client: &mut Client, directory: &Directory, n: usize) -> ClientMsg {
    let mut doc = client.document().clone();
    doc.images.insert(key(n), ImageData { bytes: Arc::new(vec![7; 1024]), width: 6000, height: 4000 });
    // The editor's bytes are the full file; the document keeps them for the local view.
    if let Some(img) = doc.images.get_mut(&key(n)) {
        img.bytes = Arc::new(vec![7; n.min(8 * MIB)]);
    }
    directory.0.lock().unwrap().insert(key(n), n as u64);
    client.mark_uploaded(&key(n), n as u64);
    client.edit(&doc).expect("edit").expect("a batch")
}

#[test]
fn batches_events_and_snapshots_no_longer_grow_with_the_image() {
    println!("\n{:>9} {:>10} {:>10} {:>14} {:>13}", "image MiB", "batch B", "event B", "snapshot B", "welcome B");
    for n in SIZES {
        let directory = Arc::new(Directory::default());
        let mut server = room(&directory);
        let mut a = join(&mut server, &message::client_version());
        let msg = paste_by_reference(&mut a, &directory, n);
        let wire = message::encode(&msg).expect("encode").len();
        let out = server.receive(a.id(), msg);
        assert!(out.iter().any(|o| matches!(o, Outgoing::All(ServerMsg::Changes(_)))), "{n}: accepted");
        let accepted = server.take_journal().into_iter().find_map(|j| if let Journal::Accepted(b) = j { Some(b) } else { None }).expect("journaled");
        let event = encode_event(tenant(), document(), &accepted).expect("a reference always fits an event");
        let state = server.state().expect("state");
        let snapshot = message::encode(&state).expect("encode").len();
        let welcome = {
            let (_, out) = server.join(hello(&message::client_version())).expect("join");
            let Some(Outgoing::To(_, w @ ServerMsg::Welcome { .. })) = out.into_iter().next() else { panic!("no welcome") };
            message::encode(&w).expect("encode").len()
        };
        println!("{:>9.1} {:>10} {:>10} {:>14} {:>13}", n as f64 / MIB as f64, wire, event.payload.len(), snapshot, welcome);
        // The size of the image is nowhere in the sizes.
        for (what, size) in [("batch", wire), ("event", event.payload.len()), ("snapshot", snapshot), ("welcome", welcome)] {
            assert!(size < 4 * KIB, "{n} bytes: the {what} is {size} bytes");
        }
        assert!(event.payload.len() < MAX_EVENT_PAYLOAD_BYTES);
        // The stored event reads back to the same batch, and the server holds a placeholder.
        let back = decode_event(tenant(), document(), event.seq, &event.payload, &event.checksum).expect("decodes");
        assert_eq!(back, accepted);
        assert!(assets::is_placeholder(server.document().images.get(&key(n)).expect("image")));
    }
}

/// What the relay does after a restart: restore the last snapshot, replay the events after it, with
/// the directory empty (the images were checked when they were accepted).
#[test]
fn a_restarted_room_keeps_its_references() {
    let directory = Arc::new(Directory::default());
    let mut server = room(&directory);
    let mut a = join(&mut server, &message::client_version());
    let first = paste_by_reference(&mut a, &directory, 30 * MIB);
    let out = server.receive(a.id(), first);
    for o in &out {
        if let Outgoing::All(m) = o {
            a.receive(m.clone(), &figcraft_collab::NoRecompute).expect("confirmed");
        }
    }
    // A snapshot (here, after the first image) and one more image after it, as events.
    let snapshot: SessionState = server.state().expect("state");
    let journal_before = server.take_journal();
    assert!(!journal_before.is_empty());
    let second = paste_by_reference(&mut a, &directory, 45 * MIB);
    let _ = server.receive(a.id(), second);
    let events: Vec<Journal> = server.take_journal();

    // The stored snapshot goes through its encoding.
    let bytes = message::encode(&snapshot).expect("encode");
    assert!(bytes.len() < 8 * KIB);
    let restored: SessionState = message::decode(&bytes, 128 * MIB).expect("decode");
    let mut again = Server::restore(restored, "tok", Limits::default()).expect("restore");
    again.set_assets(Arc::new(Directory::default()), AssetPolicy::default());
    for event in events {
        again.replay(event).expect("replays without the directory");
    }
    assert_eq!(again.seq(), server.seq());
    assert_eq!(again.document().images.len(), 2);
    assert!(again.document().images.values().all(assets::is_placeholder));

    // A client that joins the restarted room is told about both, and what it has to fetch.
    let late = join(&mut again, &message::client_version());
    let missing = late.missing_images();
    assert_eq!(missing.len(), 2);
    assert!(missing.iter().all(|m| (m.width, m.height) == (6000, 4000)));
}

/// An older client in the same room: it never sees a reference, sends its images inline, and a
/// batch the event log could not keep is refused with a reason instead of looping.
#[test]
fn an_older_client_keeps_working_inline_within_the_old_limits() {
    let directory = Arc::new(Directory::default());
    let mut server = room(&directory);
    let old_words = "0.3.0 signals hosting";
    let mut old = join(&mut server, old_words);
    let mut new = join(&mut server, &message::client_version());
    // New client's reference: the old one is sent the batch without it.
    let reference = paste_by_reference(&mut new, &directory, 30 * MIB);
    let out = server.receive(new.id(), reference);
    let to_old = out
        .iter()
        .find_map(|o| match o {
            Outgoing::To(id, ServerMsg::Changes(a)) if *id == old.id() => Some(a.clone()),
            _ => None,
        })
        .expect("the old client is told of the batch");
    assert!(!assets::has_refs(&to_old.changes));
    old.receive(ServerMsg::Changes(to_old), &figcraft_collab::NoRecompute).expect("applies");

    // The old client pastes 4 MiB: inline, accepted, stored.
    let mut doc = old.document().clone();
    doc.images.insert(key(4), ImageData { bytes: Arc::new(vec![3; 4 * MIB]), width: 10, height: 10 });
    let msg = old.edit(&doc).expect("edit").expect("batch");
    let out = server.receive(old.id(), msg);
    assert!(out.iter().any(|o| matches!(o, Outgoing::All(ServerMsg::Changes(_)) | Outgoing::To(_, ServerMsg::Changes(_)))));
    let accepted =
        server.take_journal().into_iter().filter_map(|j| if let Journal::Accepted(b) = j { Some(b) } else { None }).next_back().expect("journaled");
    assert!(encode_event(tenant(), document(), &accepted).is_ok());

    // ... and 12 MiB: refused with a reason, once.
    let mut doc = old.document().clone();
    doc.images.insert(key(12), ImageData { bytes: Arc::new(vec![5; 12 * MIB]), width: 10, height: 10 });
    let msg = old.edit(&doc).expect("edit").expect("batch");
    let out = server.receive(old.id(), msg);
    let reason = out.iter().find_map(|o| if let Outgoing::To(_, ServerMsg::Reject { reason, .. }) = o { Some(reason.clone()) } else { None });
    assert!(reason.is_some_and(|r| r.contains("MB")));
}
