//! Other people's drags drawn headless: the layers' boxes at their live position with "<name> is
//! moving", until they let go, leave, or stop refreshing.

use figcraft_doc::NodeId;
use figcraft_engine::live::{DragBox, Event as LiveEvent, Signal};

use crate::live_tests::{Harness, has_text};

/// Joined as Ada with Hana here, and the host reads signals.
fn ready() -> (Harness, NodeId) {
    let (mut h, rect) = Harness::new();
    h.frames(3);
    h.joined();
    h.send(vec![LiveEvent::Signal(2, Signal::ready())]);
    h.frames(2);
    assert!(h.app.session.live().signals);
    (h, rect)
}

fn moving(id: NodeId, x: f32) -> Signal {
    Signal::drag(&[DragBox { id, x, y: 100.0, w: 200.0, h: 100.0 }])
}

#[test]
fn a_drag_in_progress_shows_with_the_name_and_goes_when_dropped() {
    let (mut h, rect) = ready();
    assert!(!has_text(&h.frames(1), "Hana is moving"));
    h.send(vec![LiveEvent::Signal(1, moving(rect, 340.0))]);
    assert!(has_text(&h.frames(2), "Hana is moving"));
    // It follows her: a new position replaces the old one, and it stays while it is refreshed.
    h.send(vec![LiveEvent::Signal(1, moving(rect, 360.0))]);
    assert!(has_text(&h.frames(2), "Hana is moving"));
    // She lets go: the preview goes at once.
    h.send(vec![LiveEvent::Signal(1, Signal::drag(&[]))]);
    assert!(!has_text(&h.frames(2), "Hana is moving"));
}

#[test]
fn a_preview_that_stops_being_refreshed_goes_stale_and_a_repeat_keeps_it() {
    let (mut h, rect) = ready();
    h.send(vec![LiveEvent::Signal(1, moving(rect, 340.0))]);
    assert!(has_text(&h.frames(2), "Hana is moving"));
    // Held still: the same boxes repeat (the sender's keepalive) and the preview stays.
    for _ in 0..4 {
        h.skip(1.0);
        h.send(vec![LiveEvent::Signal(1, moving(rect, 340.0))]);
        assert!(has_text(&h.frames(2), "Hana is moving"), "repeats keep it");
    }
    // Nothing for longer than the stale limit: hidden, though no "let go" arrived.
    h.skip(3.5);
    assert!(!has_text(&h.frames(2), "Hana is moving"));
}

#[test]
fn only_layers_you_have_on_this_page_are_drawn_and_leaving_removes_it() {
    let (mut h, rect) = ready();
    // A layer your copy doesn't have (not synced yet, or another page's): nothing to draw.
    h.send(vec![LiveEvent::Signal(1, moving(NodeId(987_654_321), 10.0))]);
    assert!(!has_text(&h.frames(2), "Hana is moving"));
    h.send(vec![LiveEvent::Signal(1, moving(rect, 340.0))]);
    assert!(has_text(&h.frames(2), "Hana is moving"));
    h.send(vec![LiveEvent::PeerLeft(1)]);
    assert!(!has_text(&h.frames(2), "Hana is moving"), "she left mid-drag");
}

#[test]
fn turning_multiplayer_cursors_off_hides_drag_previews_too() {
    let (mut h, rect) = ready();
    h.send(vec![LiveEvent::Signal(1, moving(rect, 340.0))]);
    assert!(has_text(&h.frames(2), "Hana is moving"));
    h.app.session.ds_mut().unwrap().view.multiplayer_cursors = false;
    assert!(!has_text(&h.frames(2), "Hana is moving"));
}
