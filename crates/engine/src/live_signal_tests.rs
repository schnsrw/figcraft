//! Cursor chat and spotlights in the engine's live state, against the scripted link.

use std::sync::{Arc, Mutex};

use figcraft_collab::message::ClientMsg;
use serde_json::json;

use crate::live::{Event, Peer, Presence, Role, Signal};
use crate::live_tests::{Script, info, push, session};
use crate::{EngineError, Session};

fn peer(id: u32, name: &str) -> Peer {
    Peer { client_id: id, name: name.into(), color: "#9747FF".into() }
}

/// The signals sent so far.
fn sent(script: &Arc<Mutex<Script>>) -> Vec<Signal> {
    script.lock().unwrap().msgs.iter().filter_map(|m| if let ClientMsg::Signal(s) = m { Some(s.clone()) } else { None }).collect()
}

/// Joined as client 2 with Hana (1) and Ada (3) in the session; `ready`: the host reads signals.
fn joined(ready: bool) -> (Session, Arc<Mutex<Script>>) {
    let (mut s, script) = session();
    s.execute("collab.join", &json!({"link": "figcraft://join?host=a:1&file=f&token=t"})).unwrap();
    let mut events = vec![Event::Welcome { info: info(Role::Guest, 2), peers: vec![peer(1, "Hana"), peer(3, "Ada")] }];
    if ready {
        events.push(Event::Signal(2, Signal::ready()));
    }
    push(&script, events);
    s.live_poll();
    (s, script)
}

#[test]
fn chat_needs_a_session_whose_host_reads_it() {
    let (mut s, _) = session();
    assert!(matches!(s.execute("collab.cursorChat", &json!({"text": "hi"})), Err(EngineError::Disabled(..))));
    // Opening the box outside a session does nothing (the `/` key stays quiet).
    assert_eq!(s.execute("collab.cursorChat", &json!({})).unwrap()["open"], false);
    assert!(s.ui_requests.is_empty());
    assert!(s.execute("collab.spotlight", &json!({"on": true})).is_err());
    let (mut s, script) = joined(false);
    let err = s.execute("collab.cursorChat", &json!({"text": "hi"})).unwrap_err().to_string();
    assert!(err.contains("older version"), "{err}");
    assert!(sent(&script).is_empty());
    // Someone else's "ready" isn't the host speaking to you.
    push(&script, vec![Event::Signal(1, Signal::ready())]);
    s.live_poll();
    assert!(!s.live().signals);
    push(&script, vec![Event::Signal(2, Signal::ready())]);
    s.live_poll();
    assert!(s.live().signals);
    assert_eq!(s.live_summary()["signals"], true);
}

#[test]
fn your_chat_goes_out_cleaned_once_per_change() {
    let (mut s, script) = joined(true);
    assert_eq!(s.execute("collab.cursorChat", &json!({})).unwrap()["open"], true);
    assert_eq!(s.ui_requests.last().map(|r| r.kind.as_str()), Some("dialog.cursorChat"));
    let r = s.execute("collab.cursorChat", &json!({"text": "hello\nthere"})).unwrap();
    assert_eq!(r["chat"], "hello there");
    s.live_chat("hello there").unwrap();
    let long = "x".repeat(500);
    assert_eq!(s.live_chat(&long).unwrap().len(), Signal::MAX_CHAT);
    s.execute("collab.cursorChat", &json!({"text": ""})).unwrap();
    s.execute("collab.cursorChat", &json!({"text": ""})).unwrap();
    let texts: Vec<String> = sent(&script).into_iter().map(|s| s.text).collect();
    assert_eq!(texts, ["hello there".to_string(), "x".repeat(Signal::MAX_CHAT), String::new()], "unchanged text isn't sent again");
    assert!(s.execute("collab.cursorChat", &json!({"text": 5})).is_err());
    assert_eq!(s.live_summary()["chat"], "");
}

#[test]
fn others_chat_shows_until_closed_or_they_leave() {
    let (mut s, script) = joined(true);
    push(
        &script,
        vec![
            Event::Signal(1, Signal::chat("hey")),
            Event::Signal(3, Signal::chat("bad\u{1b}[2J")),
            // Not in the session, unknown kinds and malformed signals: ignored.
            Event::Signal(9, Signal::chat("ghost")),
            Event::Signal(1, Signal { kind: "highFive".into(), text: String::new() }),
            Event::Signal(1, Signal { kind: "spotlight".into(), text: "maybe".into() }),
        ],
    );
    s.live_poll();
    assert_eq!(s.live().chats.get(&1).map(String::as_str), Some("hey"));
    assert_eq!(s.live().chats.get(&3).map(String::as_str), Some("bad [2J"), "control characters never reach the canvas");
    assert_eq!(s.live().chats.len(), 2);
    assert_eq!(s.live().spotlight, None);
    let peers = s.live_summary()["peers"].clone();
    assert_eq!(peers[0]["chat"], "hey");
    push(&script, vec![Event::Signal(1, Signal::chat("")), Event::PeerLeft(3)]);
    s.live_poll();
    assert!(s.live().chats.is_empty());
}

#[test]
fn a_spotlight_makes_you_follow_until_you_move() {
    let (mut s, script) = joined(true);
    push(&script, vec![Event::Signal(1, Signal::spotlight(true)), Event::Presence(1, Presence::default())]);
    s.live_poll();
    assert_eq!((s.live().spotlight, s.live().follow), (Some(1), Some(1)));
    assert!(s.message.take().unwrap().contains("Hana is spotlighting"));
    // Moving your view stops following; the spotlight goes on (the banner offers to follow again).
    push(&script, vec![Event::Presence(1, Presence { viewport: Some((s.page().unwrap(), 0.0, 0.0, 600.0, 400.0)), ..Default::default() })]);
    s.live_poll();
    s.ds_mut().unwrap().view.cx += 50.0;
    s.live_sync();
    assert_eq!((s.live().spotlight, s.live().follow), (Some(1), None));
    s.execute("collab.follow", &json!({"client": 1})).unwrap();
    // Ada takes over; then she stops and nobody is followed.
    push(&script, vec![Event::Signal(3, Signal::spotlight(true))]);
    s.live_poll();
    assert_eq!((s.live().spotlight, s.live().follow), (Some(3), Some(3)));
    push(&script, vec![Event::Signal(1, Signal::spotlight(false))]);
    s.live_poll();
    assert_eq!(s.live().spotlight, Some(3), "an old presenter stopping changes nothing");
    push(&script, vec![Event::Signal(3, Signal::spotlight(false))]);
    s.live_poll();
    assert_eq!((s.live().spotlight, s.live().follow), (None, None));
    // The presenter leaving ends it too.
    push(&script, vec![Event::Signal(1, Signal::spotlight(true)), Event::PeerLeft(1)]);
    s.live_poll();
    assert_eq!((s.live().spotlight, s.live().follow), (None, None));
}

#[test]
fn spotlighting_yourself() {
    let (mut s, script) = joined(true);
    s.execute("collab.follow", &json!({"client": 1})).unwrap();
    assert_eq!(s.execute("collab.spotlight", &json!({})).unwrap()["spotlighting"], true);
    assert_eq!(s.live().follow, None, "a presenter follows nobody");
    s.execute("collab.spotlight", &json!({"on": true})).unwrap();
    assert_eq!(sent(&script), vec![Signal::spotlight(true)], "on twice is sent once");
    assert!(s.execute("collab.spotlight", &json!({"on": "yes"})).is_err());
    // Someone else's spotlight takes over yours.
    push(&script, vec![Event::Signal(3, Signal::spotlight(true))]);
    s.live_poll();
    assert!(!s.live().spotlighting);
    assert_eq!(s.live().follow, Some(3));
    s.execute("collab.spotlight", &json!({"on": true})).unwrap();
    // A reconnect: the host hears it again once it reads signals again.
    push(&script, vec![Event::Status(crate::live::Status::Reconnecting { attempt: 1 })]);
    s.live_poll();
    assert!(!s.live().signals && s.live().spotlighting);
    push(&script, vec![Event::Welcome { info: info(Role::Guest, 2), peers: vec![peer(1, "Hana")] }, Event::Signal(2, Signal::ready())]);
    s.live_poll();
    assert_eq!(sent(&script).last(), Some(&Signal::spotlight(true)));
    assert_eq!(sent(&script).len(), 3);
    s.execute("collab.spotlight", &json!({})).unwrap();
    assert_eq!(sent(&script).last(), Some(&Signal::spotlight(false)));
    // Leaving forgets it all.
    s.execute("collab.leave", &json!({})).unwrap();
    assert!(!s.live().spotlighting && !s.live().signals && s.live().chat.is_empty());
}

// ---- drags in progress ----

use crate::live::{DRAG_INTERVAL, DRAG_KEEPALIVE, DragBox};
use crate::tools::{PointerEvent, PointerKind};
use figcraft_doc::NodeId;

/// The drag signals sent so far, as their boxes (empty: "let go").
fn drags_sent(script: &Arc<Mutex<Script>>) -> Vec<Vec<DragBox>> {
    sent(script).into_iter().filter(|s| s.kind == Signal::DRAG).map(|s| s.drag_boxes().unwrap()).collect()
}

fn rect_in_session() -> (Session, Arc<Mutex<Script>>, NodeId) {
    let (mut s, script) = joined(true);
    let id = NodeId(s.execute("shape.rectangle", &json!({"x": 100, "y": 100, "width": 40, "height": 20})).unwrap()["id"].as_u64().unwrap());
    s.execute("select.set", &json!({"ids": [id]})).unwrap();
    (s, script, id)
}

fn at(kind: PointerKind, x: f64, y: f64) -> PointerEvent {
    PointerEvent::at(kind, x, y, 1.0)
}

#[test]
fn your_drag_streams_while_it_moves_is_throttled_repeated_and_ends_when_you_let_go() {
    let (mut s, script, rect) = rect_in_session();
    // Nothing to say before a drag.
    s.live_stream_drag(0.0);
    assert!(drags_sent(&script).is_empty());

    s.pointer(&at(PointerKind::Down, 120.0, 110.0)).unwrap();
    s.pointer(&at(PointerKind::Move, 130.0, 110.0)).unwrap();
    s.pointer(&at(PointerKind::Move, 150.0, 110.0)).unwrap();
    s.live_stream_drag(10.0);
    let first = drags_sent(&script);
    assert_eq!(first.len(), 1);
    let b = first[0][0];
    // The box is where the layer is now (moved 30 right), not where it started.
    assert_eq!((b.id, b.x, b.y, b.w, b.h), (rect, 130.0, 100.0, 40.0, 20.0));

    // Moving on within the interval waits; past it, the new position goes out.
    s.pointer(&at(PointerKind::Move, 160.0, 110.0)).unwrap();
    s.live_stream_drag(10.0 + DRAG_INTERVAL / 2.0);
    assert_eq!(drags_sent(&script).len(), 1, "throttled");
    s.live_stream_drag(10.0 + DRAG_INTERVAL * 1.5);
    let sent_now = drags_sent(&script);
    assert_eq!(sent_now.len(), 2);
    assert_eq!(sent_now[1][0].x, 140.0);

    // Holding still: nothing new until the keepalive.
    s.live_stream_drag(10.0 + DRAG_INTERVAL * 3.0);
    assert_eq!(drags_sent(&script).len(), 2);
    s.live_stream_drag(10.0 + DRAG_INTERVAL * 1.5 + DRAG_KEEPALIVE + 0.01);
    assert_eq!(drags_sent(&script).len(), 3, "repeated so the others' preview doesn't time out");

    // Letting go tells them once, and then nothing more.
    s.pointer(&at(PointerKind::Up, 160.0, 110.0)).unwrap();
    s.live_stream_drag(20.0);
    let all = drags_sent(&script);
    assert_eq!(all.len(), 4);
    assert!(all[3].is_empty(), "an empty drag: let go");
    s.live_stream_drag(20.5);
    s.live_stream_drag(30.0);
    assert_eq!(drags_sent(&script).len(), 4);
}

#[test]
fn a_drag_is_not_streamed_without_a_host_that_passes_signals_on_or_to_a_viewer_or_for_a_marquee() {
    // The host doesn't read signals: nothing goes out (and nothing breaks).
    let (mut s, script) = joined(false);
    let id = NodeId(s.execute("shape.rectangle", &json!({"x": 100, "y": 100, "width": 40, "height": 20})).unwrap()["id"].as_u64().unwrap());
    s.execute("select.set", &json!({"ids": [id]})).unwrap();
    s.pointer(&at(PointerKind::Down, 120.0, 110.0)).unwrap();
    s.pointer(&at(PointerKind::Move, 150.0, 110.0)).unwrap();
    s.live_stream_drag(1.0);
    assert!(sent(&script).is_empty());
    s.pointer(&at(PointerKind::Up, 150.0, 110.0)).unwrap();

    // A marquee (a drag on empty canvas) moves no layer: no preview.
    let (mut s, script, _) = rect_in_session();
    s.pointer(&at(PointerKind::Down, 300.0, 300.0)).unwrap();
    s.pointer(&at(PointerKind::Move, 340.0, 340.0)).unwrap();
    s.live_stream_drag(1.0);
    assert!(drags_sent(&script).is_empty());
    s.pointer(&at(PointerKind::Up, 340.0, 340.0)).unwrap();
}

#[test]
fn others_drags_show_while_they_last_and_go_when_they_end_leave_or_the_connection_drops() {
    let (mut s, script) = joined(true);
    let one = DragBox { id: NodeId(9), x: 1.0, y: 2.0, w: 30.0, h: 40.0 };
    let two = DragBox { id: NodeId(9), x: 5.0, y: 2.0, w: 30.0, h: 40.0 };
    push(&script, vec![Event::Signal(1, Signal::drag(&[one]))]);
    s.live_poll();
    assert_eq!(s.live().drags.get(&1), Some(&vec![one]));
    // The latest wins, and the end removes it.
    push(&script, vec![Event::Signal(1, Signal::drag(&[two]))]);
    s.live_poll();
    assert_eq!(s.live().drags.get(&1), Some(&vec![two]));
    push(&script, vec![Event::Signal(1, Signal::drag(&[]))]);
    s.live_poll();
    assert!(s.live().drags.is_empty());
    // A peer who leaves mid-drag takes the preview with them.
    push(&script, vec![Event::Signal(3, Signal::drag(&[one]))]);
    s.live_poll();
    assert!(s.live().drags.contains_key(&3));
    push(&script, vec![Event::PeerLeft(3)]);
    s.live_poll();
    assert!(s.live().drags.is_empty());
    // Not your own, not a stranger's, and nothing malformed.
    push(&script, vec![Event::Signal(2, Signal::drag(&[one])), Event::Signal(77, Signal::drag(&[one]))]);
    push(&script, vec![Event::Signal(1, Signal { kind: Signal::DRAG.into(), text: "1,2,3".into() })]);
    s.live_poll();
    assert!(s.live().drags.is_empty());
    // The connection dropping clears them.
    push(&script, vec![Event::Signal(1, Signal::drag(&[one]))]);
    s.live_poll();
    assert!(!s.live().drags.is_empty());
    push(&script, vec![Event::Status(crate::live::Status::Reconnecting { attempt: 1 })]);
    s.live_poll();
    assert!(s.live().drags.is_empty());
}
