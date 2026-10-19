//! FigCraft Cloud (fork-owned): open the cloud file this page is for. Same-origin requests carry the
//! browser's session cookie (never visible here) and the organisation the file browser chose
//! (`localStorage["figcraft.tenant"]`, sent as `X-FigCraft-Tenant`). The API stays the authority:
//! this only reads the file's name, project, share permission and live-session invite.

use std::cell::RefCell;
use std::rc::Rc;

use figcraft_ui_egui::cloud::{CloudFile, Problem};
use wasm_bindgen::JsCast as _;

use crate::cloud_route::{self, Failure, FileRoute};

/// Where the file browser keeps the chosen organisation (`webapp/saas/src/session.tsx`).
const TENANT_KEY: &str = "figcraft.tenant";
const API: &str = "/api/v1";

/// The result of opening the file, handed to the app on its next frame.
pub enum Opened {
    /// The file's metadata and the invite link its live session is joined with.
    Ready { file: CloudFile, invite: String },
    /// It can't be opened; the page says why.
    Failed(CloudFile),
}

pub type Slot = Rc<RefCell<Option<Opened>>>;

/// The page's file before anything has loaded: where Back to files and Share lead.
pub fn initial(route: &FileRoute) -> CloudFile {
    let origin = web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default();
    CloudFile { files_url: route.files_path(), link: format!("{origin}{}", route.path()), ..CloudFile::default() }
}

/// Load the file's metadata and invite into `slot`, waking the page when done. A signed-out
/// browser goes to sign-in, which comes back to this page.
pub fn open(route: FileRoute, slot: Slot, wake: egui::Context) {
    wasm_bindgen_futures::spawn_local(async move {
        let mut file = initial(&route);
        let result = load(&route, &mut file).await;
        let opened = match result {
            Ok(invite) => Opened::Ready { file, invite },
            Err(Failure::SignIn) => {
                if let Some(w) = web_sys::window()
                    && let Err(e) = w.location().assign(&cloud_route::sign_in_url(&route.path()))
                {
                    log::error!("couldn't go to sign-in: {e:?}");
                }
                return;
            }
            Err(Failure::NoAccess) => Opened::Failed(CloudFile { problem: Some(Problem::NoAccess), ..file }),
            Err(Failure::Unavailable) => Opened::Failed(CloudFile { problem: Some(Problem::Unavailable), ..file }),
        };
        *slot.borrow_mut() = Some(opened);
        wake.request_repaint();
    });
}

async fn load(route: &FileRoute, file: &mut CloudFile) -> Result<String, Failure> {
    let tenant = tenant();
    let doc = get(&format!("{API}/documents/{}", route.document), tenant.as_deref()).await?;
    let meta = cloud_route::document(&doc).ok_or(Failure::NoAccess)?;
    file.name = meta.name;
    file.can_share = meta.can_share;
    // The project's name is a label: the file opens without it.
    if let Some(project) = meta.project_id
        && let Ok(body) = get(&format!("{API}/projects/{project}"), tenant.as_deref()).await
    {
        file.project = cloud_route::project_name(&body).unwrap_or_default();
    }
    let link = get(&format!("{API}/documents/{}/collab-link", route.document), tenant.as_deref()).await?;
    cloud_route::invite(&link).ok_or(Failure::Unavailable)
}

/// The organisation the file browser chose, if it is an id.
fn tenant() -> Option<String> {
    let storage = web_sys::window()?.local_storage().ok()??;
    storage.get_item(TENANT_KEY).ok()?.filter(|t| cloud_route::is_uuid(t))
}

/// `GET url` (same origin, with the session cookie) as text, at most [`cloud_route::MAX_BODY`].
async fn get(url: &str, tenant: Option<&str>) -> Result<String, Failure> {
    let window = web_sys::window().ok_or(Failure::Unavailable)?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_credentials(web_sys::RequestCredentials::SameOrigin);
    let headers = web_sys::Headers::new().map_err(|_| Failure::Unavailable)?;
    headers.set("Accept", "application/json").map_err(|_| Failure::Unavailable)?;
    if let Some(t) = tenant {
        headers.set("X-FigCraft-Tenant", t).map_err(|_| Failure::Unavailable)?;
    }
    init.set_headers(&headers);
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|_| Failure::Unavailable)?;
    let response: web_sys::Response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .ok()
        .and_then(|r| r.dyn_into().ok())
        .ok_or(Failure::Unavailable)?;
    if !response.ok() {
        return Err(cloud_route::failure(response.status()));
    }
    let text = response.text().map_err(|_| Failure::Unavailable)?;
    let body = wasm_bindgen_futures::JsFuture::from(text).await.ok().and_then(|b| b.as_string()).ok_or(Failure::Unavailable)?;
    if body.len() > cloud_route::MAX_BODY {
        return Err(Failure::Unavailable);
    }
    Ok(body)
}
