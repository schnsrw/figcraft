//! FigCraft Cloud editor mode, drawn headless: no Home and no tabs while the file opens (or when it
//! can't), Back to files from the failure page, the main menu and the project name, local file
//! commands hidden from shortcuts and the Actions palette, and Share copying the file's link.

use std::sync::{Arc, Mutex};

use egui::{Event, PointerButton, Pos2};
use figcraft_engine::Session;

use crate::Services;
use crate::cloud::{CloudFile, Problem};
use crate::layers_drag_tests::Ui;
use crate::live_tests::{flat, has_text, text_center};
use crate::menus::{PaletteTab, palette_sections};

#[derive(Default)]
struct Seen {
    navigated: Vec<String>,
    copied: Vec<String>,
    downloads: usize,
}

struct Browser(Arc<Mutex<Seen>>);

impl Services for Browser {
    fn navigate(&mut self, url: &str) {
        self.0.lock().unwrap().navigated.push(url.to_string());
    }
    fn clipboard_set_text(&mut self, s: &str) {
        self.0.lock().unwrap().copied.push(s.to_string());
    }
    fn downloads(&self) -> bool {
        true
    }
    fn download(&mut self, _name: &str, _bytes: &[u8]) -> bool {
        self.0.lock().unwrap().downloads += 1;
        true
    }
}

fn file() -> CloudFile {
    CloudFile {
        name: "Checkout flow".into(),
        project: "Mobile app".into(),
        files_url: "/workspace/11111111-1111-4111-8111-111111111111".into(),
        link: "https://design.example/workspace/11111111-1111-4111-8111-111111111111/file/22222222-2222-4222-8222-222222222222".into(),
        can_share: true,
        problem: None,
    }
}

fn cloud_ui(session: Session) -> (Ui, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut u = Ui::new(session);
    u.app.services = Box::new(Browser(seen.clone()));
    u.app.cloud = Some(file());
    (u, seen)
}

fn shapes(u: &mut Ui) -> Vec<egui::Shape> {
    flat(&u.frame_output(vec![]).shapes)
}

fn click(u: &mut Ui, p: Pos2) {
    for pressed in [true, false] {
        u.frame(vec![
            Event::PointerMoved(p),
            Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Default::default() },
        ]);
    }
}

#[test]
fn an_opening_file_shows_its_name_and_never_home() {
    let (mut u, _) = cloud_ui(Session::empty());
    let s = shapes(&mut u);
    assert!(has_text(&s, "Opening Checkout flow…"));
    assert!(!has_text(&s, "Recents") && !has_text(&s, "Templates"), "no home screen");
    // Asking for Home (a menu, the control channel) changes nothing.
    u.app.ui.home = true;
    let s = shapes(&mut u);
    assert!(!u.app.ui.home);
    assert!(!has_text(&s, "Recents"));
}

#[test]
fn a_file_that_cannot_open_leads_back_to_the_files() {
    let (mut u, seen) = cloud_ui(Session::empty());
    u.app.cloud.as_mut().unwrap().problem = Some(Problem::NoAccess);
    let s = shapes(&mut u);
    assert!(has_text(&s, "Couldn't open this file"));
    assert!(has_text(&s, "It doesn't exist, or you don't have access to it."));
    click(&mut u, text_center(&s, "Back to files"));
    assert_eq!(seen.lock().unwrap().navigated, vec!["/workspace/11111111-1111-4111-8111-111111111111".to_string()]);
}

#[test]
fn local_file_commands_do_nothing_and_save_says_it_is_automatic() {
    let (mut u, seen) = cloud_ui(Session::new());
    let files = u.app.session.docs.len();
    for id in crate::cloud::HIDDEN {
        crate::menus::invoke(&mut u.app, id);
    }
    u.frame(vec![]);
    assert_eq!(u.app.session.docs.len(), files, "no file opened, duplicated or closed");
    assert!(!u.app.ui.home);
    assert_eq!(seen.lock().unwrap().downloads, 0, "Save downloads nothing");
    assert!(u.app.ui.live.join.is_none() && u.app.ui.live.publish.is_none(), "no live-session dialogs");
    assert_eq!(u.app.ui.toast.as_ref().map(|(t, _)| t.as_str()), Some("Changes are saved automatically"));
}

#[test]
fn the_actions_palette_leaves_out_local_file_commands() {
    let commands = |u: &Ui, q: &str| -> Vec<String> {
        palette_sections(&u.app, q, PaletteTab::All).into_iter().flat_map(|(_, rows)| rows).map(|r| r.key).collect()
    };
    let mut local = Ui::new(Session::new());
    local.frame(vec![]);
    assert!(commands(&local, "save").iter().any(|k| k == "command:file.save"), "a local file can be saved");
    let (u, _) = cloud_ui(Session::new());
    let keys = commands(&u, "save");
    assert!(!keys.iter().any(|k| k.strip_prefix("command:").is_some_and(crate::cloud::hides)), "{keys:?}");
    assert!(commands(&u, "export").iter().any(|k| k == "command:file.export"), "export stays");
}

#[test]
fn the_main_menu_and_the_project_name_go_back_to_the_files() {
    let (mut u, seen) = cloud_ui(Session::new());
    let s = shapes(&mut u);
    assert!(has_text(&s, "Mobile app"), "the project under the file name");
    assert!(!has_text(&s, "Drafts"));
    click(&mut u, text_center(&s, "Mobile app"));
    u.app.ui.main_menu = true;
    // A new menu area sizes itself on its first frame and shows on the next.
    u.frame(vec![]);
    let s = shapes(&mut u);
    click(&mut u, text_center(&s, "Back to files"));
    let navigated = seen.lock().unwrap().navigated.clone();
    assert_eq!(navigated.len(), 2, "{navigated:?}");
    assert!(navigated.iter().all(|n| n == "/workspace/11111111-1111-4111-8111-111111111111"));
    assert!(!u.app.ui.main_menu, "the menu closes");
}

#[test]
fn share_copies_the_file_link() {
    let (mut u, seen) = cloud_ui(Session::new());
    let s = shapes(&mut u);
    click(&mut u, text_center(&s, "Share"));
    let s = shapes(&mut u);
    assert!(has_text(&s, "Share this file"));
    click(&mut u, text_center(&s, "Copy link"));
    assert_eq!(seen.lock().unwrap().copied, vec![file().link]);
}

#[test]
fn without_a_cloud_file_nothing_is_hidden() {
    let mut u = Ui::new(Session::new());
    u.frame(vec![]);
    assert!(!crate::cloud::blocks(&u.app, "file.save"));
    let s = shapes(&mut u);
    assert!(has_text(&s, "Drafts"), "a local file is a draft");
}
