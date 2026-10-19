//! Exporting a page where the platform saves by downloads (the browser, which has no save dialogs):
//! the PNG arrives as a download named after the page instead of nothing happening.

use std::sync::{Arc, Mutex};

use figcraft_engine::Session;
use serde_json::json;

use crate::export_panel::save_one;
use crate::{FigApp, Services};

type Downloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Browser {
    got: Downloads,
    deliver: bool,
}

impl Services for Browser {
    fn downloads(&self) -> bool {
        true
    }
    fn download(&mut self, name: &str, bytes: &[u8]) -> bool {
        self.got.lock().unwrap().push((name.to_string(), bytes.to_vec()));
        self.deliver
    }
}

fn app(deliver: bool) -> (FigApp, Downloads) {
    let mut session = Session::new();
    session.execute("shape.rectangle", &json!({"x": 10, "y": 10, "width": 100, "height": 50})).unwrap();
    let got = Downloads::default();
    (FigApp::new(session, Box::new(Browser { got: got.clone(), deliver })), got)
}

fn toast(app: &FigApp) -> String {
    app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default()
}

#[test]
fn a_page_export_downloads_a_png() {
    let (mut app, got) = app(true);
    let page = app.session.page().unwrap();
    let ids = app.session.doc().unwrap().get(page).unwrap().children.clone();
    save_one(&mut app, &ids, &figcraft_doc::ExportSetting::default(), "Page 1.png");
    let got = got.lock().unwrap();
    let [(name, bytes)] = got.as_slice() else { panic!("one download, got {}", got.len()) };
    assert_eq!(name, "Page 1.png");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a real PNG");
    assert!(toast(&app).contains("Page 1.png"), "{}", toast(&app));
}

#[test]
fn a_refused_download_says_so() {
    let (mut app, _got) = app(false);
    let page = app.session.page().unwrap();
    let ids = app.session.doc().unwrap().get(page).unwrap().children.clone();
    save_one(&mut app, &ids, &figcraft_doc::ExportSetting::default(), "Page 1.png");
    assert!(toast(&app).to_lowercase().contains("couldn"), "{}", toast(&app));
}
