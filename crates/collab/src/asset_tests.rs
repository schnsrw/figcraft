//! Images by reference (FP-013): the value encoding, the capability handshake, the server's
//! checks, what a client that cannot read references is sent, the client's upload-then-reference
//! flow and hydration, and the batch-size refusal that ends the retry loop of oversized inline
//! images.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use figcraft_doc::{Document, ImageData};

use crate::assets::{self, AssetPolicy};
use crate::message::{self, ClientMsg, Hello, IMAGE_REFS, ServerMsg, Signal};
use crate::replica::{Mode, Replica};
use crate::{AssetDirectory, CatchUp, Change, Client, Entry, Json, Limits, NoRecompute, Notice, Outgoing, Server};

const MIB: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct Store(Mutex<BTreeMap<String, u64>>);

impl Store {
    fn put(&self, hash: &str, len: u64) {
        self.0.lock().unwrap().insert(hash.to_string(), len);
    }
}

impl AssetDirectory for Store {
    fn verified(&self, hash: &str, len: u64) -> bool {
        self.0.lock().unwrap().get(hash).is_some_and(|l| *l == len)
    }
}

fn hash(n: u64) -> String {
    format!("{n:064x}")
}

fn bytes(n: usize, seed: u8) -> Arc<Vec<u8>> {
    Arc::new((0..n).map(|i| (i as u8).wrapping_add(seed)).collect())
}

fn hello(words: &str) -> Hello {
    Hello {
        protocol: message::PROTOCOL,
        file: "f".into(),
        token: "tok".into(),
        name: "Ada".into(),
        color: "#FF8800".into(),
        client_version: words.into(),
        resume: None,
    }
}

fn modern() -> String {
    message::client_version()
}

fn old() -> String {
    "0.3.0 signals hosting".into()
}

fn server(store: &Arc<Store>) -> Server {
    let mut s = Server::new(Document::new("Shared"), "tok", Limits::default());
    s.set_assets(store.clone(), AssetPolicy::default());
    s
}

fn join(s: &mut Server, words: &str) -> (Client, Vec<Outgoing>) {
    let (_, out) = s.join(hello(words)).unwrap();
    let Some(Outgoing::To(_, welcome)) = out.first().cloned() else { panic!("no welcome") };
    (Client::join(welcome).unwrap(), out)
}

/// Deliver the server's output addressed to `c`.
fn deliver(c: &mut Client, out: &[Outgoing]) {
    let id = c.id();
    for o in out {
        let m = match o {
            Outgoing::To(t, m) if *t == id => m,
            Outgoing::All(m) => m,
            Outgoing::Others(not, m) if *not != id => m,
            _ => continue,
        };
        c.receive(m.clone(), &NoRecompute).unwrap();
    }
}

fn image_doc(c: &Client, key: &str, len: usize) -> Document {
    let mut d = c.document().clone();
    d.images.insert(key.into(), ImageData { bytes: bytes(len, 1), width: 4000, height: 3000 });
    d
}

fn assets_signal(out: &[Outgoing]) -> Option<AssetPolicy> {
    out.iter().find_map(|o| match o {
        Outgoing::To(_, ServerMsg::Signal { signal, .. }) => signal.asset_policy(),
        _ => None,
    })
}

// ---- the value ----------------------------------------------------------------------------

#[test]
fn an_inline_image_keeps_its_old_shape_and_a_reference_has_its_own() {
    let img = ImageData { bytes: Arc::new(vec![1, 2, 3]), width: 2, height: 3 };
    let mut a = Document::new("A");
    a.images.insert("k".into(), img);
    let mut puts = Vec::new();
    crate::docprops::diff(&Document::new("A"), &a, &mut puts).unwrap();
    let Some(Change::Put { value, .. }) = puts.first() else { panic!("no put") };
    // Exactly what older builds wrote and read: width, height, bytes.
    assert_eq!(value.0, serde_json::json!({"width": 2, "height": 3, "bytes": "AQID"}));
    let r = assets::ref_change(&hash(1), 4000, 3000, 31_457_280).unwrap();
    let Change::Put { value, .. } = &r else { panic!("not a put") };
    assert_eq!(value.0, serde_json::json!({"width": 4000, "height": 3000, "asset": {"len": 31_457_280u64}}));
    assert_eq!(assets::image_ref(&r), Some(assets::ImageRef { width: 4000, height: 3000, len: 31_457_280 }));
    assert_eq!(assets::image_ref(&Change::Put { entry: Entry::Image("k".into()), value: value.clone() }).map(|r| r.len), Some(31_457_280));
    // Binary and JSON forms round-trip it.
    let msg = ClientMsg::Changes { base_seq: 0, batch_id: 1, changes: vec![r.clone()] };
    assert_eq!(message::decode::<ClientMsg>(&message::encode(&msg).unwrap(), 1 << 20).unwrap(), msg);
    assert_eq!(message::decode_json::<ClientMsg>(&message::encode_json(&msg).unwrap(), 1 << 20).unwrap(), msg);
    assert!(message::encode(&msg).unwrap().len() < 300, "a 30 MiB image is a few hundred bytes");
}

#[test]
fn a_value_needs_exactly_one_of_bytes_and_asset() {
    let mut r = Replica::new(Document::new("A"));
    let both = Json(serde_json::json!({"width": 1, "height": 1, "bytes": "AQID", "asset": {"len": 3}}));
    let neither = Json(serde_json::json!({"width": 1, "height": 1}));
    for value in [both, neither] {
        let put = Change::Put { entry: Entry::Image(hash(1)), value };
        assert!(r.apply(&[put], Mode::Strict).is_err());
    }
}

#[test]
fn a_reference_is_a_placeholder_that_never_replaces_bytes() {
    let mut r = Replica::new(Document::new("A"));
    r.apply(&[assets::ref_change(&hash(1), 8, 6, 100).unwrap()], Mode::Strict).unwrap();
    let held = r.document().images.get(&hash(1)).unwrap();
    assert!(assets::is_placeholder(held) && (held.width, held.height) == (8, 6));
    // The bytes arrive inline (an old client's put), then a reference to the same content.
    let inline = Json(serde_json::json!({"width": 8, "height": 6, "bytes": "AQID"}));
    r.apply(&[Change::Put { entry: Entry::Image(hash(1)), value: inline }], Mode::Strict).unwrap();
    r.apply(&[assets::ref_change(&hash(1), 8, 6, 3).unwrap()], Mode::Strict).unwrap();
    assert_eq!(r.document().images.get(&hash(1)).unwrap().bytes.as_slice(), &[1, 2, 3]);
}

#[test]
fn a_placeholder_and_its_bytes_are_the_same_image_to_a_diff() {
    let mut with = Document::new("A");
    with.images.insert(hash(1), ImageData { bytes: bytes(1000, 0), width: 8, height: 6 });
    let mut without = Document::new("A");
    without.images.insert(hash(1), assets::placeholder(8, 6));
    let mut out = Vec::new();
    crate::docprops::diff(&with, &without, &mut out).unwrap();
    crate::docprops::diff(&without, &with, &mut out).unwrap();
    assert!(out.is_empty(), "{out:?}");
    // A different size is a different image.
    without.images.insert(hash(1), assets::placeholder(9, 6));
    crate::docprops::diff(&with, &without, &mut out).unwrap();
    assert_eq!(out.len(), 1);
}

#[test]
fn the_assets_signal_parses_and_is_cleaned() {
    let p = AssetPolicy { threshold: 262_144, max_bytes: 104_857_600 };
    assert_eq!(Signal::assets(&p).asset_policy(), Some(p));
    assert!(Signal::assets(&p).clean().is_some());
    for bad in ["", "1", "a,b", "5,0", "10,5", "1,2,3", "-1,5"] {
        let s = Signal { kind: Signal::ASSETS.into(), text: bad.into() };
        assert!(s.asset_policy().is_none() && s.clean().is_none(), "{bad:?}");
    }
}

// ---- the handshake ------------------------------------------------------------------------

#[test]
fn only_a_client_that_announced_it_is_told_the_policy() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (_, out) = s.join(hello(&old())).unwrap();
    assert!(assets_signal(&out).is_none(), "an old client never gets the signal");
    let (_, out) = s.join(hello(&modern())).unwrap();
    assert_eq!(assets_signal(&out), Some(AssetPolicy::default()));
    assert!(message::reads_image_refs(&modern()) && !message::reads_image_refs(&old()));
    // Signals alone don't announce it.
    let (_, out) = s.join(hello("1 signals")).unwrap();
    assert!(assets_signal(&out).is_none());
    // A server without assets (the filesystem server, a host) never sends it.
    let mut plain = Server::new(Document::new("Shared"), "tok", Limits::default());
    let (_, out) = plain.join(hello(&modern())).unwrap();
    assert!(assets_signal(&out).is_none(), "a server that holds no assets doesn't offer references");
}

#[test]
fn a_client_cannot_send_the_assets_signal() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (a, _) = join(&mut s, &modern());
    let (b, _) = join(&mut s, &format!("{} signals", IMAGE_REFS));
    let sig = Signal::assets(&AssetPolicy { threshold: 1, max_bytes: 2 });
    assert!(s.receive(a.id(), ClientMsg::Signal(sig)).is_empty());
    let _ = b;
}

// ---- the server's checks ------------------------------------------------------------------

fn reference(c: &mut Client, key: &str, len: usize) -> ClientMsg {
    let d = image_doc(c, key, len);
    for (k, _) in c.images_to_upload(&d, assets::DEFAULT_THRESHOLD) {
        c.mark_uploaded(&k, len as u64);
    }
    c.edit(&d).unwrap().unwrap()
}

fn reject_reason(out: &[Outgoing]) -> Option<String> {
    out.iter().find_map(|o| match o {
        Outgoing::To(_, ServerMsg::Reject { reason, .. }) => Some(reason.clone()),
        _ => None,
    })
}

#[test]
fn the_server_accepts_only_references_to_verified_assets() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, _) = join(&mut s, &modern());
    // Not uploaded: refused, with the reason.
    let msg = reference(&mut a, &hash(7), 30 * MIB);
    let out = s.receive(a.id(), msg);
    let why = reject_reason(&out).expect("rejected");
    assert!(why.contains("wasn't uploaded"), "{why}");
    deliver(&mut a, &out);
    assert!(a.take_notices().iter().any(|n| matches!(n, Notice::Rejected { .. })));
    assert_eq!(a.pending(), 0, "the client does not keep resending it");
    assert!(!s.document().images.contains_key(&hash(7)));

    // Uploaded with another length: the same.
    store.put(&hash(8), 5);
    let msg = reference(&mut a, &hash(8), 30 * MIB);
    assert!(reject_reason(&s.receive(a.id(), msg)).is_some_and(|w| w.contains("wasn't uploaded")));

    // Uploaded: accepted, and the server holds a placeholder, not bytes.
    store.put(&hash(9), (30 * MIB) as u64);
    let msg = reference(&mut a, &hash(9), 30 * MIB);
    let out = s.receive(a.id(), msg);
    assert!(reject_reason(&out).is_none(), "{out:?}");
    assert!(assets::is_placeholder(s.document().images.get(&hash(9)).unwrap()));
    assert!(s.replica().snapshot().unwrap().iter().all(|c| crate::server::weight(std::slice::from_ref(c)) < 4096), "the snapshot carries no bytes");
}

#[test]
fn bad_references_are_refused() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (a, _) = join(&mut s, &modern());
    let batch = std::cell::Cell::new(0);
    let send = |s: &mut Server, c: Change| {
        batch.set(batch.get() + 1);
        reject_reason(&s.receive(a.id(), ClientMsg::Changes { base_seq: 0, batch_id: batch.get(), changes: vec![c] }))
    };
    store.put("not-a-hash", 10);
    store.put(&hash(1), 10);
    store.put(&hash(2), assets::DEFAULT_MAX_BYTES + 1);
    // Not a SHA-256 key.
    assert!(send(&mut s, assets::ref_change("not-a-hash", 1, 1, 10).unwrap()).is_some_and(|w| w.contains("SHA-256")));
    // Zero or too many bytes, absurd sizes.
    assert!(send(&mut s, assets::ref_change(&hash(1), 1, 1, 0).unwrap()).is_some());
    assert!(send(&mut s, assets::ref_change(&hash(2), 1, 1, assets::DEFAULT_MAX_BYTES + 1).unwrap()).is_some());
    assert!(send(&mut s, assets::ref_change(&hash(1), 0, 1, 10).unwrap()).is_some());
    assert!(send(&mut s, assets::ref_change(&hash(1), 1, assets::MAX_SIDE + 1, 10).unwrap()).is_some());
    assert!(send(&mut s, assets::ref_change(&hash(1), 1, 1, 10).unwrap()).is_none(), "the same, valid");
}

#[test]
fn a_client_that_did_not_announce_references_cannot_send_one() {
    let store = Arc::new(Store::default());
    store.put(&hash(1), 10);
    let mut s = server(&store);
    let (a, _) = join(&mut s, &old());
    let change = assets::ref_change(&hash(1), 1, 1, 10).unwrap();
    let out = s.receive(a.id(), ClientMsg::Changes { base_seq: 0, batch_id: 1, changes: vec![change.clone()] });
    assert!(reject_reason(&out).is_some_and(|w| w.contains("announced")));
    // And a server with no assets takes none either.
    let mut plain = Server::new(Document::new("Shared"), "tok", Limits::default());
    let (b, _) = join(&mut plain, &modern());
    let out = plain.receive(b.id(), ClientMsg::Changes { base_seq: 0, batch_id: 1, changes: vec![change] });
    assert!(reject_reason(&out).is_some_and(|w| w.contains("inline only")));
}

// ---- who is sent what ---------------------------------------------------------------------

#[test]
fn clients_that_cannot_read_references_are_sent_batches_and_snapshots_without_them() {
    let store = Arc::new(Store::default());
    store.put(&hash(1), 300_000);
    let mut s = server(&store);
    let (mut new, _) = join(&mut s, &modern());
    let (mut old_a, _) = join(&mut s, &old());
    let msg = reference(&mut new, &hash(1), 300_000);
    let out = s.receive(new.id(), msg);
    // Everyone gets a batch with the same sequence number; the old client's has no reference.
    let batch_for = |id: u32| {
        out.iter().find_map(|o| match o {
            Outgoing::To(t, ServerMsg::Changes(a)) if *t == id => Some(a.clone()),
            _ => None,
        })
    };
    let to_new = batch_for(new.id()).unwrap();
    let to_old = batch_for(old_a.id()).unwrap();
    assert_eq!((to_new.seq, to_old.seq), (1, 1));
    assert!(assets::has_refs(&to_new.changes) && !assets::has_refs(&to_old.changes));
    deliver(&mut new, &out);
    deliver(&mut old_a, &out);
    assert_eq!(new.seq(), 1);
    assert_eq!(old_a.seq(), 1);
    assert!(!old_a.document().images.contains_key(&hash(1)), "it never learns of an image it cannot read");

    // Joining later: the same split in the snapshot.
    let (late_old, _) = join(&mut s, &old());
    assert!(!late_old.document().images.contains_key(&hash(1)));
    let (late_new, _) = join(&mut s, &modern());
    assert!(late_new.document().images.contains_key(&hash(1)));
    assert_eq!(late_new.missing_images().iter().map(|m| m.hash.clone()).collect::<Vec<_>>(), vec![hash(1)]);

    // Reconnecting with a delta: the same.
    let (id, out) = {
        let ClientMsg::Hello(h) = old_a.reconnect(hello(&old())) else { panic!() };
        s.join(h).unwrap()
    };
    assert_eq!(id, old_a.id());
    let Some(Outgoing::To(_, ServerMsg::Welcome { catch_up, .. })) = out.first() else { panic!("no welcome") };
    match catch_up {
        CatchUp::Delta(batches) => assert!(batches.iter().all(|b| !assets::has_refs(&b.changes))),
        CatchUp::Snapshot(c) => assert!(!assets::has_refs(c)),
    }
}

// ---- the client ---------------------------------------------------------------------------

#[test]
fn upload_then_reference_then_fetch() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, out) = join(&mut s, &modern());
    let (mut b, _) = join(&mut s, &modern());
    let policy = assets_signal(&out).unwrap();

    let key = hash(42);
    let data = bytes(2 * MIB, 3);
    let d = image_doc(&a, &key, 2 * MIB);
    // 1. The client finds what to upload.
    let todo = a.images_to_upload(&d, policy.threshold);
    assert_eq!(todo.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec![key.as_str()]);
    assert!(a.images_to_upload(&image_doc(&a, "short", 2 * MIB), policy.threshold).is_empty(), "not content-addressed: stays inline");
    assert!(a.images_to_upload(&image_doc(&a, &hash(5), 1000), policy.threshold).is_empty(), "under the threshold: stays inline");
    // 2. It uploads, and the edit is small.
    store.put(&key, data.len() as u64);
    a.mark_uploaded(&key, data.len() as u64);
    let msg = a.edit(&d).unwrap().unwrap();
    assert!(message::encode(&msg).unwrap().len() < 400);
    let out = s.receive(a.id(), msg);
    deliver(&mut a, &out);
    deliver(&mut b, &out);
    assert_eq!(a.pending(), 0);
    // The author keeps its own bytes after the server's confirmation, and after others' edits.
    assert_eq!(a.document().images.get(&key).unwrap().bytes.len(), 2 * MIB);
    assert!(a.missing_images().is_empty());
    // 3. The other client has a placeholder and fetches.
    assert_eq!(b.missing_images(), vec![assets::MissingImage { hash: key.clone(), width: 4000, height: 3000 }]);
    assert!(!b.set_image_bytes(&hash(99), data.clone()), "not an image of the document");
    assert!(b.set_image_bytes(&key, data.clone()));
    assert!(b.missing_images().is_empty());
    assert_eq!(b.document().images.get(&key).unwrap().bytes, data);
    // 4. Neither resends the bytes on its next edits, which diff as no change to the image.
    for author_is_a in [true, false] {
        let c = if author_is_a { &mut a } else { &mut b };
        let doc = c.document().clone();
        assert!(c.edit(&doc).unwrap().is_none());
        let mut doc = c.document().clone();
        doc.name = format!("renamed by {}", c.id());
        let msg = c.edit(&doc).unwrap().unwrap();
        let ClientMsg::Changes { changes, .. } = &msg else { panic!() };
        assert!(changes.iter().all(|ch| !matches!(ch, Change::Put { entry: Entry::Image(_), .. })), "{changes:?}");
        let out = s.receive(c.id(), msg);
        deliver(&mut a, &out);
        deliver(&mut b, &out);
    }
    assert_eq!(a.document().images.get(&key).unwrap().bytes.len(), 2 * MIB);
    assert_eq!(b.document().images.get(&key).unwrap().bytes.len(), 2 * MIB);
    assert_eq!(a.document().name, s.document().name);
}

#[test]
fn a_reconnect_with_a_snapshot_keeps_the_bytes_a_client_holds() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, _) = join(&mut s, &modern());
    let key = hash(11);
    store.put(&key, (MIB) as u64);
    let d = image_doc(&a, &key, MIB);
    a.mark_uploaded(&key, MIB as u64);
    let msg = a.edit(&d).unwrap().unwrap();
    let out = s.receive(a.id(), msg);
    deliver(&mut a, &out);
    a.disconnect();
    s.leave(a.id());
    // So many batches pass that the log no longer reaches it: it catches up with a snapshot.
    let mut tiny = Server::restore(s.state().unwrap(), "tok", Limits { max_log_batches: 1, ..Limits::default() }).unwrap();
    tiny.set_assets(store.clone(), AssetPolicy::default());
    let (b, _) = join(&mut tiny, &modern());
    let mut b = b;
    for i in 0..3 {
        let mut doc = b.document().clone();
        doc.name = format!("n{i}");
        let msg = b.edit(&doc).unwrap().unwrap();
        let out = tiny.receive(b.id(), msg);
        deliver(&mut b, &out);
    }
    let ClientMsg::Hello(h) = a.reconnect(hello(&modern())) else { panic!() };
    let (_, out) = tiny.join(h).unwrap();
    let Some(Outgoing::To(_, welcome @ ServerMsg::Welcome { .. })) = out.first().cloned() else { panic!() };
    a.receive(welcome, &NoRecompute).unwrap();
    assert_eq!(a.document().name, "n2");
    assert_eq!(a.document().images.get(&key).unwrap().bytes.len(), MIB, "its own bytes are not lost to the snapshot's placeholder");
}

// ---- the retry loop -----------------------------------------------------------------------

#[test]
fn an_oversized_inline_batch_is_refused_not_accepted_and_lost() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    s.set_max_batch_bytes(Some(15 * MIB));
    // An old client pastes a 12 MiB image: 16 MiB of base64.
    let (mut old_client, _) = join(&mut s, &old());
    let msg = {
        let d = image_doc(&old_client, &hash(3), 12 * MIB);
        old_client.edit(&d).unwrap().unwrap()
    };
    let out = s.receive(old_client.id(), msg);
    let why = reject_reason(&out).expect("refused");
    assert!(why.contains("MB") && why.contains("by reference"), "{why}");
    assert_eq!(s.seq(), 0);
    assert!(s.take_journal().is_empty());
    deliver(&mut old_client, &out);
    assert_eq!(old_client.pending(), 0, "the author is told once and stops");
    assert!(matches!(old_client.take_notices().first(), Some(Notice::Rejected { .. })));
    assert!(!old_client.document().images.contains_key(&hash(3)), "the refused image is rolled back");
    // 5 MiB inline still goes through (legacy clients keep working up to the limit).
    let msg = {
        let d = image_doc(&old_client, &hash(4), 5 * MIB);
        old_client.edit(&d).unwrap().unwrap()
    };
    assert!(reject_reason(&s.receive(old_client.id(), msg)).is_none());
}

#[test]
fn replaying_a_journal_does_not_recheck_assets() {
    let store = Arc::new(Store::default());
    store.put(&hash(1), 300_000);
    let mut s = server(&store);
    s.journaling(true);
    let (mut a, _) = join(&mut s, &modern());
    let msg = reference(&mut a, &hash(1), 300_000);
    assert!(reject_reason(&s.receive(a.id(), msg)).is_none());
    let journal = s.take_journal();
    // After a restart the directory is empty (or unreachable) and the author is gone: the batch
    // was accepted once, and replays.
    let mut again = Server::new(Document::new("Shared"), "tok", Limits::default());
    again.set_assets(Arc::new(Store::default()), AssetPolicy::default());
    for j in journal {
        again.replay(j).unwrap();
    }
    assert_eq!(again.seq(), 1);
    assert!(assets::is_placeholder(again.document().images.get(&hash(1)).unwrap()));
}

// ---- batches that wait for an upload ------------------------------------------------------

fn batch_ids(msgs: &[ClientMsg]) -> Vec<u64> {
    msgs.iter().filter_map(|m| if let ClientMsg::Changes { batch_id, .. } = m { Some(*batch_id) } else { None }).collect()
}

#[test]
fn a_batch_waits_for_its_image_to_be_uploaded_and_so_does_everything_after_it() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, _) = join(&mut s, &modern());
    let key = hash(21);
    let len = 3 * MIB;
    let d = image_doc(&a, &key, len);
    a.expect_upload(&key, len as u64);
    // The edit is made at once (the view shows the image) but nothing goes out.
    assert!(a.edit(&d).unwrap().is_none());
    assert_eq!(a.pending(), 1);
    assert_eq!(a.document().images.get(&key).unwrap().bytes.len(), len);
    let mut renamed = a.document().clone();
    renamed.name = "while uploading".into();
    assert!(a.edit(&renamed).unwrap().is_none(), "a later edit queues behind it");
    assert_eq!(a.pending(), 2);
    assert!(a.images_to_upload(&image_doc(&a, &hash(22), len), 1).iter().all(|(k, _)| k != &key));
    // The upload finishes: both go out, in order, the first as a small reference.
    store.put(&key, len as u64);
    let out = a.mark_uploaded(&key, len as u64);
    assert_eq!(batch_ids(&out).len(), 2, "{out:?}");
    assert!(message::encode(&out[0]).unwrap().len() < 400);
    for m in out {
        let reply = s.receive(a.id(), m);
        assert!(reject_reason(&reply).is_none(), "{reply:?}");
        deliver(&mut a, &reply);
    }
    assert_eq!(a.pending(), 0);
    assert_eq!(s.document().name, "while uploading");
    assert!(assets::is_placeholder(s.document().images.get(&key).unwrap()));
    assert_eq!(a.document().images.get(&key).unwrap().bytes.len(), len, "the author keeps its bytes");
}

#[test]
fn an_upload_that_cannot_be_made_sends_the_bytes_inline() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, _) = join(&mut s, &modern());
    let key = hash(23);
    let d = image_doc(&a, &key, MIB);
    a.expect_upload(&key, MIB as u64);
    assert!(a.edit(&d).unwrap().is_none());
    let out = a.abandon_upload(&key);
    assert_eq!(out.len(), 1);
    let ClientMsg::Changes { changes, .. } = &out[0] else { panic!("not a batch") };
    assert!(changes.iter().any(|c| matches!(c, Change::Put { entry: Entry::Image(_), value } if value.0.get("bytes").is_some())), "{changes:?}");
    let reply = s.receive(a.id(), out[0].clone());
    assert!(reject_reason(&reply).is_none());
    assert_eq!(s.document().images.get(&key).unwrap().bytes.len(), MIB, "the server holds the bytes it was sent");
}

#[test]
fn a_reconnect_keeps_waiting_batches_waiting() {
    let store = Arc::new(Store::default());
    let mut s = server(&store);
    let (mut a, _) = join(&mut s, &modern());
    let key = hash(24);
    let d = image_doc(&a, &key, MIB);
    a.expect_upload(&key, MIB as u64);
    assert!(a.edit(&d).unwrap().is_none());
    a.disconnect();
    s.leave(a.id());
    let ClientMsg::Hello(h) = a.reconnect(hello(&modern())) else { panic!() };
    let (_, out) = s.join(h).unwrap();
    let Some(Outgoing::To(_, welcome)) = out.first().cloned() else { panic!() };
    assert!(a.receive(welcome, &NoRecompute).unwrap().is_empty(), "nothing goes out before the upload is done");
    store.put(&key, MIB as u64);
    let out = a.mark_uploaded(&key, MIB as u64);
    assert_eq!(out.len(), 1);
    assert!(reject_reason(&s.receive(a.id(), out[0].clone())).is_none());
}
