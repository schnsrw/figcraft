//! Signals (cursor chat, spotlight): who gets them, how the server cleans them, and what a
//! newcomer is told.

use figcraft_doc::Document;

use crate::message::{ClientMsg, Hello, ServerMsg, Signal, client_version, reads_signals};
use crate::{Client, Limits, Outgoing, Server};

fn hello(version: &str) -> Hello {
    let ClientMsg::Hello(h) = Client::hello("f", "", "Ada", "#FF8800") else { panic!("not a hello") };
    Hello { client_version: version.into(), ..h }
}

fn server() -> Server {
    Server::new(Document::new("Shared"), "", Limits::default())
}

/// The signals among `out` for client `to`, as (from, signal).
fn signals(out: &[Outgoing], to: u32) -> Vec<(u32, Signal)> {
    out.iter()
        .filter_map(|o| match o {
            Outgoing::To(t, ServerMsg::Signal { client_id, signal }) if *t == to => Some((*client_id, signal.clone())),
            Outgoing::All(ServerMsg::Signal { .. }) | Outgoing::Others(_, ServerMsg::Signal { .. }) => panic!("signals go to readers only"),
            _ => None,
        })
        .collect()
}

#[test]
fn the_client_version_announces_signals() {
    assert!(reads_signals(&client_version()));
    assert!(reads_signals("0.4.0 signals"));
    assert!(!reads_signals("0.3.1"));
    assert!(!reads_signals("0.3.1 signalsx"));
}

#[test]
fn only_clients_that_read_signals_get_them() {
    let mut s = server();
    let (new, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, new), vec![(new, Signal::ready())], "the server says it takes signals, after the welcome");
    assert!(matches!(out.first(), Some(Outgoing::To(_, ServerMsg::Welcome { .. }))));
    let (old, out) = s.join(hello("0.3.1")).unwrap();
    assert!(signals(&out, old).is_empty(), "an older client can't read them");
    let (other, _) = s.join(hello(&client_version())).unwrap();
    // The old client's chat reaches the readers; nothing goes back to it or to the sender.
    let out = s.receive(old, ClientMsg::Signal(Signal::chat("hi")));
    assert_eq!(signals(&out, new), vec![(old, Signal::chat("hi"))]);
    assert_eq!(signals(&out, other), vec![(old, Signal::chat("hi"))]);
    assert_eq!(out.len(), 2);
    let out = s.receive(new, ClientMsg::Signal(Signal::chat("yo")));
    assert!(signals(&out, old).is_empty() && signals(&out, new).is_empty());
    assert_eq!(signals(&out, other), vec![(new, Signal::chat("yo"))]);
    // Gone: no more signals for it.
    s.leave(other);
    assert!(s.receive(new, ClientMsg::Signal(Signal::chat("still here?"))).is_empty());
}

#[test]
fn signals_are_cleaned_and_unknown_ones_dropped() {
    let mut s = server();
    let (a, _) = s.join(hello(&client_version())).unwrap();
    let (b, _) = s.join(hello(&client_version())).unwrap();
    let send = |s: &mut Server, sig: Signal| signals(&s.receive(a, ClientMsg::Signal(sig)), b);
    // Control characters become spaces; long messages are cut to the limit (in characters).
    let got = send(&mut s, Signal::chat("one\ntwo\u{7}\u{202e}"));
    assert_eq!(got[0].1.text, "one two \u{202e}");
    let long = "é".repeat(300);
    let got = send(&mut s, Signal::chat(&long));
    assert_eq!(got[0].1.text.chars().count(), Signal::MAX_CHAT);
    // Blank is closed.
    assert_eq!(send(&mut s, Signal::chat(" \t ")), vec![(a, Signal::chat(""))]);
    // Unknown kinds, oversized ones, a client's "ready" and malformed spotlights go nowhere.
    for junk in [
        Signal { kind: "highFive".into(), text: String::new() },
        Signal { kind: "chat".into(), text: "x".repeat(Signal::MAX_BYTES * 4) },
        Signal::ready(),
        Signal { kind: "spotlight".into(), text: "yes".into() },
        Signal { kind: String::new(), text: String::new() },
    ] {
        assert!(s.receive(a, ClientMsg::Signal(junk.clone())).is_empty(), "{junk:?}");
    }
}

#[test]
fn a_newcomer_learns_who_spotlights() {
    let mut s = server();
    let (a, _) = s.join(hello(&client_version())).unwrap();
    s.receive(a, ClientMsg::Signal(Signal::spotlight(true)));
    let (b, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, b), vec![(b, Signal::ready()), (a, Signal::spotlight(true))]);
    // Someone else's spotlight takes over; stopping one that isn't current changes nothing.
    s.receive(b, ClientMsg::Signal(Signal::spotlight(true)));
    s.receive(a, ClientMsg::Signal(Signal::spotlight(false)));
    let (c, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, c), vec![(c, Signal::ready()), (b, Signal::spotlight(true))]);
    // The presenter leaving ends it.
    s.leave(b);
    let (d, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, d), vec![(d, Signal::ready())]);
    // Stopping it does too.
    s.receive(c, ClientMsg::Signal(Signal::spotlight(true)));
    s.receive(c, ClientMsg::Signal(Signal::spotlight(false)));
    let (e, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, e), vec![(e, Signal::ready())]);
}

#[test]
fn signals_never_reach_the_document_or_the_log() {
    let mut s = server();
    s.journaling(true);
    let (a, _) = s.join(hello(&client_version())).unwrap();
    s.take_journal();
    let seq = s.seq();
    let before = s.state().unwrap();
    s.receive(a, ClientMsg::Signal(Signal::chat("hello")));
    s.receive(a, ClientMsg::Signal(Signal::spotlight(true)));
    assert_eq!(s.seq(), seq);
    let after = s.state().unwrap();
    assert_eq!(after.snapshot, before.snapshot);
    assert_eq!(after.log, before.log);
    assert!(s.take_journal().is_empty(), "nothing for the store to keep");
}

fn boxed(id: u64, x: f32) -> crate::message::DragBox {
    crate::message::DragBox { id: figcraft_doc::NodeId(id), x, y: -3.5, w: 120.25, h: 40.0 }
}

#[test]
fn a_drag_signal_carries_boxes_and_round_trips() {
    let boxes = vec![boxed(7, 10.5), boxed(u64::MAX, -1e6)];
    let sig = Signal::drag(&boxes);
    assert_eq!(sig.kind, Signal::DRAG);
    assert_eq!(sig.drag_boxes(), Some(boxes));
    // No boxes: the drag ended.
    assert_eq!(Signal::drag(&[]).drag_boxes(), Some(Vec::new()));
    assert_eq!(Signal::drag(&[]).text, "");
    // Not a drag signal at all.
    assert_eq!(Signal::chat("hi").drag_boxes(), None);
}

#[test]
fn a_drag_signal_is_bounded_and_never_carries_what_is_not_finite() {
    let many: Vec<_> = (0..50).map(|i| boxed(i, 0.0)).collect();
    let sig = Signal::drag(&many);
    assert_eq!(sig.drag_boxes().map(|b| b.len()), Some(Signal::MAX_DRAG_BOXES), "a preview keeps the first few");
    assert!(sig.kind.len() + sig.text.len() <= Signal::MAX_BYTES);
    // Even the widest numbers fit.
    let wide: Vec<_> = (0..Signal::MAX_DRAG_BOXES as u64)
        .map(|i| crate::message::DragBox { id: figcraft_doc::NodeId(u64::MAX - i), x: f32::MIN, y: f32::MIN_POSITIVE, w: f32::MAX, h: f32::MAX })
        .collect();
    let sig = Signal::drag(&wide);
    assert!(sig.kind.len() + sig.text.len() <= Signal::MAX_BYTES);
    assert!(sig.clone().clean().is_some());
    // Non-finite boxes are left out when sending.
    assert_eq!(Signal::drag(&[boxed(1, f32::NAN), boxed(2, 0.0)]).drag_boxes().map(|b| b.len()), Some(1));
}

#[test]
fn malformed_drag_signals_are_refused_by_the_server() {
    let mut s = server();
    let (a, _) = s.join(hello(&client_version())).unwrap();
    let (b, _) = s.join(hello(&client_version())).unwrap();
    for text in [
        "1,2,3,4",     // a field short
        "1,2,3,4,5,6", // a field over
        "x,2,3,4,5",   // the id isn't a number
        "1,NaN,3,4,5", // not finite
        "1,inf,3,4,5",
        "1,2,3,-4,5", // a negative size
        "1,2,3,4,5;", // an empty box
        ";",
        "1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5;1,2,3,4,5", // 13 boxes
    ] {
        let out = s.receive(a, ClientMsg::Signal(Signal { kind: Signal::DRAG.into(), text: text.into() }));
        assert!(signals(&out, b).is_empty(), "{text:?} must go nowhere");
    }
}

#[test]
fn a_drag_goes_to_readers_of_signals_only_and_is_not_kept_for_newcomers() {
    let mut s = server();
    let (a, _) = s.join(hello(&client_version())).unwrap();
    let (reader, _) = s.join(hello(&client_version())).unwrap();
    let (old, _) = s.join(hello("0.3.1")).unwrap();
    let sig = Signal::drag(&[boxed(5, 1.0)]);
    let out = s.receive(a, ClientMsg::Signal(sig.clone()));
    assert_eq!(signals(&out, reader), vec![(a, sig.clone())]);
    assert!(signals(&out, old).is_empty() && signals(&out, a).is_empty());
    assert_eq!(out.len(), 1);
    // A newcomer is told nothing of a drag in progress (it is a moment, not state).
    let (late, out) = s.join(hello(&client_version())).unwrap();
    assert_eq!(signals(&out, late), vec![(late, Signal::ready())]);
    // Ending the drag goes out too.
    let out = s.receive(a, ClientMsg::Signal(Signal::drag(&[])));
    assert_eq!(signals(&out, reader).len(), 1);
}

#[test]
fn a_viewer_cannot_show_a_drag_but_can_still_chat() {
    let mut s = server();
    let (editor, _) = s.join(hello(&client_version())).unwrap();
    let (viewer, _) = s.join_as(hello(&client_version()), crate::message::Access::View).unwrap();
    let drag = Signal::drag(&[boxed(5, 1.0)]);
    // The viewer's drag goes nowhere; the editor's reaches the viewer.
    assert!(signals(&s.receive(viewer, ClientMsg::Signal(drag.clone())), editor).is_empty());
    assert_eq!(signals(&s.receive(editor, ClientMsg::Signal(drag.clone())), viewer), vec![(editor, drag)]);
    // Chat is for everyone.
    assert_eq!(signals(&s.receive(viewer, ClientMsg::Signal(Signal::chat("hi"))), editor), vec![(viewer, Signal::chat("hi"))]);
}
