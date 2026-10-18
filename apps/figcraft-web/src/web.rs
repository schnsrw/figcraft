//! The browser shell: web `Services` and the eframe web runner.

use figcraft_engine::Session;
use figcraft_ui_egui::{FigApp, Services};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast as _;

use crate::branding::{self, Branding};

const CANVAS_ID: &str = "figcraft_canvas";
const LOADING_ID: &str = "figcraft_loading";
/// Served by the FigCraft Cloud web container (docs/saas/branding-and-configuration.md).
const BRANDING_URL: &str = "/branding.json";

/// Startup branding shared by the branding fetch and the runner task.
#[derive(Default)]
struct Startup {
    branding: Branding,
    /// The runner finished (started or failed): the loading element is no longer ours to change.
    done: bool,
}

pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    let startup = Rc::new(RefCell::new(Startup::default()));
    // The deployment's `/branding.json` (FigCraft Cloud renders it from the web container's
    // environment); a failure or a page served without it keeps the default name.
    let brand_state = Rc::clone(&startup);
    wasm_bindgen_futures::spawn_local(async move {
        let Some(body) = fetch_text(BRANDING_URL).await else { return };
        let brand = branding::parse(&body);
        let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
        document.set_title(&brand.title());
        let mut state = brand_state.borrow_mut();
        if !state.done
            && let Some(el) = document.get_element_by_id(LOADING_ID)
        {
            el.set_text_content(Some(&brand.loading_text()));
        }
        state.branding = brand;
    });
    wasm_bindgen_futures::spawn_local(async move {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let mut options = eframe::WebOptions::default();
        // `?webgl`: force the WebGL2 backend (troubleshooting, and headless browsers whose WebGPU
        // adapter request returns nothing).
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(|cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("figcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
                    }
                    let reduce_motion = web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten());
                    let mut session = Session::new();
                    // Live sessions: the browser joins sessions a desktop app hosts; whatever arrives
                    // on the socket wakes the page for a frame.
                    let ctx = cc.egui_ctx.clone();
                    session.live.install(Box::new(figcraft_collab_net::NetLink::new(std::sync::Arc::new(move || ctx.request_repaint()))));
                    let mut fig = FigApp::new(session, Box::new(WebServices { reduce_motion, ctx: cc.egui_ctx.clone() }));
                    // `#join=<link>`: join that session, as opening an invite link does on the desktop.
                    let hash = web_sys::window().and_then(|w| w.location().hash().ok()).unwrap_or_default();
                    let mut pending_name = None;
                    if let Some(link) = crate::query::join_link(&hash) {
                        let link = js_sys::decode_uri_component(link).ok().and_then(|s| s.as_string()).unwrap_or_else(|| link.to_string());
                        pending_name = crate::query::join_name(&hash)
                            .and_then(|name| js_sys::decode_uri_component(name).ok().and_then(|s| s.as_string()))
                            .map(|name| name.trim().chars().take(MAX_FILE_NAME_CHARS).collect::<String>())
                            .filter(|name| !name.is_empty());
                        figcraft_ui_egui::live::open_invite(&mut fig, &link);
                        clear_join_fragment();
                    }
                    Ok(Box::new(WebShell { fig, pending_name }))
                }),
            )
            .await;
        let mut state = startup.borrow_mut();
        state.done = true;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                // Text only: the product name comes from the deployment, never as markup.
                Err(e) => el.set_text_content(Some(&state.branding.failure_text(&format!("{e:?}")))),
            }
        }
    });
}

/// `GET <url>` (relative to the page origin) as text; `None` on any failure, non-2xx status or a
/// body over [`branding::MAX_BRANDING_BYTES`]. Uses `Response` through `js_sys::Reflect` so the
/// crate needs no extra web-sys features.
async fn fetch_text(url: &str) -> Option<String> {
    let window = web_sys::window()?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.ok()?;
    if !js_sys::Reflect::get(&response, &"ok".into()).ok()?.as_bool()? {
        return None;
    }
    let text: js_sys::Function = js_sys::Reflect::get(&response, &"text".into()).ok()?.dyn_into().ok()?;
    let promise: js_sys::Promise = text.call0(&response).ok()?.dyn_into().ok()?;
    let body = wasm_bindgen_futures::JsFuture::from(promise).await.ok()?.as_string()?;
    (body.len() <= branding::MAX_BRANDING_BYTES).then_some(body)
}

/// FigCraft Cloud fork: drop a `#join` fragment from the address bar once it has been read
/// (`history.replaceState`, so no history entry keeps the link's secret).
fn clear_join_fragment() {
    let Some(window) = web_sys::window() else { return };
    let location = window.location();
    let without = format!("{}{}", location.pathname().unwrap_or_default(), location.search().unwrap_or_default());
    if let Ok(history) = window.history()
        && let Err(e) = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&without))
    {
        log::warn!("couldn't clear the join link from the address bar: {e:?}");
    }
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

struct WebShell {
    fig: FigApp,
    /// The name the workspace gave the cloud file this page joined (`#name=…&join=…`), applied once
    /// if the joined file still has the placeholder name.
    pending_name: Option<String>,
}

/// The placeholder every new cloud file starts with (`figcraft_cloud_relay::collab_backend::BASE_NAME`).
const PLACEHOLDER_NAME: &str = "Untitled";
/// Longest file name taken from the page address, in characters.
const MAX_FILE_NAME_CHARS: usize = 80;

impl WebShell {
    /// A new cloud file's document is the same placeholder for everyone, whatever the workspace
    /// calls the file. Once the session is live and its document has arrived, the first person in
    /// gives it the workspace's name as an ordinary rename: it syncs, is saved with the file and
    /// everyone sees it. A file that already has a name of its own is left alone.
    fn apply_cloud_name(&mut self) {
        if self.pending_name.is_none() {
            return;
        }
        let live = self.fig.session.live();
        if live.status != figcraft_engine::live::Status::Live || live.info.is_none() || live.doc_uid.is_none() {
            return;
        }
        let placeholder = self.fig.session.doc().is_ok_and(|doc| doc.name == PLACEHOLDER_NAME);
        if let Some(name) = self.pending_name.take()
            && placeholder
        {
            self.fig.run("file.rename", serde_json::json!({ "name": name }));
        }
    }
}

impl eframe::App for WebShell {
    // `FigApp::ui` runs its own per-frame logic first (fonts, theme), like the desktop app.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.apply_cloud_name();
        self.fig.ui(ui);
    }
}

/// Browser services. Open dialogs return nothing for now (the trait's defaults); files are saved
/// as downloads. Copies go to the system clipboard through egui's output; pastes arrive as paste
/// events with their text (a page can't read the clipboard when it likes).
struct WebServices {
    /// `prefers-reduced-motion: reduce` (it follows the setting while the page is open).
    reduce_motion: Option<web_sys::MediaQueryList>,
    ctx: egui::Context,
}

impl Services for WebServices {
    fn downloads(&self) -> bool {
        true
    }
    /// A Blob behind a temporary `<a download>` link, clicked and removed.
    fn download(&mut self, name: &str, bytes: &[u8]) -> bool {
        match download(name, bytes) {
            Ok(()) => true,
            Err(e) => {
                log::error!("couldn't download {name}: {e:?}");
                false
            }
        }
    }
    fn clipboard_set_text(&mut self, s: &str) {
        figcraft_ui_egui::egui_copy_text(&self.ctx, s);
    }
    fn clipboard_set_png(&mut self, w: u32, h: u32, rgba: &[u8]) {
        figcraft_ui_egui::egui_copy_png(&self.ctx, w, h, rgba);
    }
    fn open_url(&mut self, url: &str) {
        let Some(window) = web_sys::window() else { return };
        if let Err(e) = window.open_with_url_and_target(url, "_blank") {
            log::error!("couldn't open {url}: {e:?}");
        }
    }
    fn reduce_motion(&self) -> bool {
        self.reduce_motion.as_ref().is_some_and(web_sys::MediaQueryList::matches)
    }
    /// The browser's language (`navigator.language`).
    fn system_language(&self) -> Option<String> {
        web_sys::window().and_then(|w| w.navigator().language())
    }
    /// The Gamepad API's controllers with the standard mapping (its button order is the layout's;
    /// others are skipped). Browsers list controllers once one of their buttons is pressed.
    fn gamepad_held(&mut self) -> u32 {
        let Some(pads) = web_sys::window().and_then(|w| w.navigator().get_gamepads().ok()) else { return 0 };
        let mut held = 0;
        for pad in pads.iter().filter_map(|p| p.dyn_into::<web_sys::Gamepad>().ok()) {
            if pad.mapping() != web_sys::GamepadMappingType::Standard {
                continue;
            }
            for (i, b) in pad.buttons().iter().take(32).enumerate() {
                if b.dyn_into::<web_sys::GamepadButton>().is_ok_and(|b| b.pressed()) {
                    held |= 1 << i;
                }
            }
        }
        held
    }
}

fn download(name: &str, bytes: &[u8]) -> Result<(), wasm_bindgen::JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let body = document.body().ok_or("no body")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &{
        let o = web_sys::BlobPropertyBag::new();
        o.set_type("application/octet-stream");
        o
    })?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a")?.dyn_into()?;
    a.set_href(&url);
    a.set_download(name);
    body.append_child(&a)?;
    a.click();
    a.remove();
    // Revoked later, not at once: some browsers cancel a download whose URL goes before it starts.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    // Without the timer the URL just stays until the page closes.
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), REVOKE_DOWNLOAD_MS);
    Ok(())
}

/// How long a download's object URL is kept (milliseconds).
const REVOKE_DOWNLOAD_MS: i32 = 40_000;
