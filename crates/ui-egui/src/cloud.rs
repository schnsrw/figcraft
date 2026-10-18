//! FigCraft Cloud editor mode (fork-owned): the page shows one cloud file, the way a hosted design
//! editor does. The file browser (workspaces, projects, files, templates, people) is the cloud web
//! app; this editor is the file only. So in this mode:
//!
//! - there is no Home screen and no tab strip (the browser tab is the file's tab);
//! - the main menu starts with "Back to files", and local file commands (Open, Save, Save as,
//!   Open recent, Close tab, Join/Publish live session…) are hidden from menus, shortcuts and the
//!   Actions palette: the cloud saves every change;
//! - the left panel names the file's project under the file name, and Share copies the file's link.
//!
//! The browser adapter (`apps/figcraft-web`) loads the file's metadata from the cloud API, fills
//! [`CloudFile`] and joins the file's live session; this module only draws and gates.

use egui::{Align2, CornerRadius, Ui, pos2, vec2};

use crate::FigApp;
use crate::i18n::{tr, trf};
use crate::theme::{Tokens, regular, semibold};
use crate::widgets;

/// Longest cloud name (file or project) shown, in characters; the API's limit is lower.
pub const MAX_NAME_CHARS: usize = 120;

/// The cloud file this page edits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CloudFile {
    /// The file's name in the cloud catalog (shown while it opens).
    pub name: String,
    /// The project it belongs to (under the file name in the left panel). Empty: not known yet.
    pub project: String,
    /// Where "Back to files" goes: the file browser at the file's workspace (a same-site path).
    pub files_url: String,
    /// The file's own address, which Share copies.
    pub link: String,
    /// The caller may manage who has access (the API's `can.share`).
    pub can_share: bool,
    /// Why the file can't be opened; the page shows it in place of the canvas.
    pub problem: Option<Problem>,
}

/// Why a cloud file can't be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Missing, archived, or not visible to the caller (the API answers 403/404 alike).
    NoAccess,
    /// The server couldn't be reached or failed.
    Unavailable,
}

/// Commands that act on local files or local live sessions. The cloud saves every change and owns
/// the file's lifetime, so these are hidden (menus, palette) and do nothing from a shortcut.
pub const HIDDEN: &[&str] = &[
    "file.new",
    "file.newFromTemplate",
    "window.new",
    "file.open",
    "file.openRecent",
    "file.save",
    "file.saveAs",
    "file.saveVersion",
    "file.versionHistory",
    "file.revert",
    "file.duplicate",
    "file.close",
    "branch.create",
    "collab.join",
    "collab.publish",
    "pref.publishServer",
];

/// Whether `id` is hidden in cloud editor mode.
pub fn hides(id: &str) -> bool {
    HIDDEN.contains(&id)
}

/// Whether the app is in cloud editor mode and `id` is hidden there.
pub fn blocks(app: &FigApp, id: &str) -> bool {
    app.cloud.is_some() && hides(id)
}

/// A hidden command run from a shortcut: Save says the cloud has it, the rest do nothing.
pub fn blocked(app: &mut FigApp, id: &str) {
    if id == "file.save" {
        app.toast(tr("Changes are saved automatically"));
    }
}

/// Main menu ▸ Back to files: the file browser, in this browser tab.
pub fn back_to_files(app: &mut FigApp) {
    let Some(url) = app.cloud.as_ref().map(|c| c.files_url.clone()) else { return };
    app.services.navigate(if url.is_empty() { "/" } else { &url });
}

/// Share ▸ Copy link.
pub fn copy_link(app: &mut FigApp) {
    let Some(link) = app.cloud.as_ref().map(|c| c.link.clone()).filter(|l| !l.is_empty()) else { return };
    app.services.clipboard_set_text(&link);
    app.toast(tr("Link copied"));
}

/// The page while the file opens, or why it can't: centred on the canvas colour, with a way back
/// to the files when it failed.
pub fn opening(app: &mut FigApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(file) = app.cloud.clone() else { return };
    let mut back = false;
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.canvas)).show(ui, |ui| {
        let r = ui.max_rect();
        let c = r.center();
        match file.problem {
            None => {
                let text =
                    if file.name.is_empty() { tr("Opening the file…").to_string() } else { trf("Opening {name}…", &[("name", &file.name)]) };
                ui.painter().text(c, Align2::CENTER_CENTER, text, regular(13.0), t.text_secondary);
            }
            Some(problem) => {
                ui.painter().text(c - vec2(0.0, 34.0), Align2::CENTER_CENTER, tr("Couldn't open this file"), semibold(15.0), t.text);
                let why = match problem {
                    Problem::NoAccess => tr("It doesn't exist, or you don't have access to it."),
                    Problem::Unavailable => tr("Couldn't reach the server. Reload the page to try again."),
                };
                ui.painter().text(c - vec2(0.0, 10.0), Align2::CENTER_CENTER, why, regular(12.0), t.text_secondary);
                let b = egui::Rect::from_center_size(pos2(c.x, c.y + 28.0), vec2(140.0, 32.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(b));
                back = widgets::button(&mut child, tr("Back to files"), true, true).clicked();
            }
        }
    });
    if back {
        back_to_files(app);
    }
}

/// The Share button's popover in cloud mode: copy the file's link (everyone the file is shared
/// with can open it; access itself is managed in the file browser).
pub fn share_popover(app: &mut FigApp, anchor: &egui::Response) {
    crate::right::menu_popup(anchor).width(260.0).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        let t = Tokens::get(ui.ctx());
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.add_space(4.0);
            ui.label(egui::RichText::new(tr("Share this file")).font(semibold(12.0)));
            widgets::label(ui, tr("People with access to this file can open its link."), t.text_secondary);
            ui.add_space(4.0);
            if widgets::button(ui, tr("Copy link"), true, true).clicked() {
                copy_link(app);
                ui.close();
            }
            ui.add_space(2.0);
        });
    });
}

/// Paint the project name under the file name (left panel header); returns whether it was clicked.
pub(crate) fn project_label(ui: &mut Ui, at: egui::Pos2, max_w: f32, project: &str) -> bool {
    let t = Tokens::get(ui.ctx());
    let galley = ui.painter().layout_no_wrap(project.to_string(), regular(11.0), t.text_secondary);
    let size = vec2(galley.size().x.min(max_w), galley.size().y);
    let r = egui::Rect::from_min_size(pos2(at.x, at.y - size.y / 2.0), size).expand2(vec2(4.0, 3.0));
    let resp = ui.interact(r, ui.id().with("cloud_project"), egui::Sense::click()).on_hover_text(tr("Back to files"));
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(4), t.hover);
    }
    crate::bidi::paint_name(ui.painter(), pos2(at.x, at.y), project, regular(11.0), t.text_secondary, max_w);
    resp.clicked()
}
