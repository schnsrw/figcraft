//! FigCraft in the browser.
//!
//! Runs the same [`figcraft_ui_egui::FigApp`] as the desktop app through eframe's web runner
//! (wgpu: WebGPU where available, WebGL2 otherwise). Build with `trunk build --release` from this
//! directory; the site lands in `dist/web` (see `packaging/web/README.md`).
//!
//! Differences from the desktop app:
//! - no TCP control server (browsers can't listen on sockets);
//! - no native file dialogs yet: the session starts with one new document;
//! - links open in a new browser tab;
//! - live sessions: it joins sessions a desktop app hosts (File ▸ Join live session…) but can't
//!   host one. A page served over `https://` can only reach sessions on the same computer
//!   (browsers block `ws://` from secure pages); serve it over `http://` on a trusted network to
//!   join from other machines.
//!
//! URL query flag: `?webgl` forces the WebGL2 backend instead of WebGPU.
//!
//! URL fragment: `#join=<url-encoded invite link>` joins that live session on startup (how a
//! FigCraft Cloud share link opens the editor at `/edit/`); the fragment is then cleared so the
//! link's secret doesn't stay in the address bar, history or bookmarks.

// The fragment parser is pure (tested on the host); only the browser shell calls it.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod hash;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("figcraft-web only runs in the browser: build it with `trunk build --release` in apps/figcraft-web");
}
