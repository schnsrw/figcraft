//! FigCraft Cloud (fork-owned): the editor page of one cloud file and the API answers it reads.
//! Pure, so it is tested on the host; the browser shell ([`crate::cloud`]) does the requests.
//!
//! The file browser links a file as `/workspace/<workspace id>/file/<document id>` (an older
//! `…/edit` suffix and a trailing slash are accepted). The web server serves this app for those
//! paths, and the app asks the cloud API for the file.

use serde_json::Value;

/// A cloud file's editor page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRoute {
    pub workspace: String,
    pub document: String,
}

impl FileRoute {
    /// The page's own path (what Share copies and sign-in comes back to).
    pub fn path(&self) -> String {
        format!("/workspace/{}/file/{}", self.workspace, self.document)
    }

    /// The file browser at the file's workspace ("Back to files").
    pub fn files_path(&self) -> String {
        format!("/workspace/{}", self.workspace)
    }
}

/// Whether `s` is a UUID in its usual lowercase or uppercase hyphenated form (what the API's ids
/// are). Anything else never reaches a request path.
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// The cloud file `path` (`location.pathname`) names, if it is a file's editor page.
pub fn file_route(path: &str) -> Option<FileRoute> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix("/edit").unwrap_or(path);
    let mut parts = path.strip_prefix('/')?.split('/');
    let (Some("workspace"), Some(workspace), Some("file"), Some(document), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (is_uuid(workspace) && is_uuid(document))
        .then(|| FileRoute { workspace: workspace.to_ascii_lowercase(), document: document.to_ascii_lowercase() })
}

/// Longest API answer read, in bytes (a document, a project or a collaboration link is far less).
pub const MAX_BODY: usize = 64 * 1024;
/// Longest name shown, in characters.
const MAX_NAME: usize = figcraft_ui_egui::cloud::MAX_NAME_CHARS;
/// Longest invite link accepted, in bytes.
const MAX_INVITE: usize = 4096;

/// `GET /api/v1/documents/{id}`: what the editor needs of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentMeta {
    pub name: String,
    pub project_id: Option<String>,
    pub can_share: bool,
}

fn name_of(v: &Value) -> Option<String> {
    let name: String = v.get("name")?.as_str()?.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    (!name.is_empty()).then_some(name)
}

/// The document answer; `None` when it is not an active document.
pub fn document(body: &str) -> Option<DocumentMeta> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("state").and_then(Value::as_str).is_some_and(|s| s != "active") {
        return None;
    }
    Some(DocumentMeta {
        name: name_of(&v).unwrap_or_default(),
        project_id: v.get("project_id").and_then(Value::as_str).filter(|id| is_uuid(id)).map(str::to_ascii_lowercase),
        can_share: v.pointer("/can/share").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// `GET /api/v1/projects/{id}`: its name.
pub fn project_name(body: &str) -> Option<String> {
    name_of(&serde_json::from_str(body).ok()?)
}

/// `GET /api/v1/documents/{id}/collab-link`: the invite link the live session is joined with.
pub fn invite(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let link = v.get("invite")?.as_str()?.trim();
    ((link.starts_with("wss://") || link.starts_with("ws://")) && link.len() <= MAX_INVITE && !link.chars().any(char::is_control))
        .then(|| link.to_string())
}

/// What a failed answer means for the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Not signed in (401): go to sign-in, which comes back here.
    SignIn,
    /// Missing or not visible (403, 404, or not an active document).
    NoAccess,
    /// Anything else: the server failed or couldn't be reached.
    Unavailable,
}

/// The meaning of an HTTP status other than 2xx.
pub fn failure(status: u16) -> Failure {
    match status {
        401 => Failure::SignIn,
        403 | 404 | 410 => Failure::NoAccess,
        _ => Failure::Unavailable,
    }
}

/// The sign-in address that comes back to `path` (a path this module built: ids and slashes).
pub fn sign_in_url(path: &str) -> String {
    let escaped: String = path.chars().map(|c| if c == '/' { "%2F".to_string() } else { c.to_string() }).collect();
    format!("/api/v1/auth/login?return_to={escaped}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "11111111-1111-4111-8111-111111111111";
    const DOC: &str = "22222222-2222-4222-8222-22222222222A";

    #[test]
    fn only_a_files_editor_path_is_a_cloud_file() {
        let want = FileRoute { workspace: WS.into(), document: DOC.to_ascii_lowercase() };
        for path in [format!("/workspace/{WS}/file/{DOC}"), format!("/workspace/{WS}/file/{DOC}/"), format!("/workspace/{WS}/file/{DOC}/edit")] {
            assert_eq!(file_route(&path).as_ref(), Some(&want), "{path}");
        }
        assert_eq!(want.path(), format!("/workspace/{WS}/file/{}", DOC.to_ascii_lowercase()));
        assert_eq!(want.files_path(), format!("/workspace/{WS}"));
        for path in [
            "/".to_string(),
            "/edit/".to_string(),
            format!("/workspace/{WS}"),
            format!("/workspace/{WS}/file/not-an-id"),
            format!("/workspace/{WS}/file/{DOC}/edit/more"),
            format!("/workspace/{WS}/file/{DOC}/x"),
            format!("/workspace/../file/{DOC}"),
            format!("workspace/{WS}/file/{DOC}"),
            format!("/workspace/{WS}//file/{DOC}"),
        ] {
            assert_eq!(file_route(&path), None, "{path}");
        }
        assert!(!is_uuid("22222222-2222-4222-8222-2222222222222"));
        assert!(!is_uuid("22222222_2222-4222-8222-222222222222"));
        assert!(!is_uuid("2222222é-2222-4222-8222-22222222222"));
    }

    #[test]
    fn the_document_answer_gives_name_project_and_share() {
        let body = format!(
            r#"{{"id":"{DOC}","workspace_id":"{WS}","project_id":"{WS}","name":"  Checkout\u0007 flow ","state":"active","can":{{"share":true}}}}"#
        );
        assert_eq!(document(&body), Some(DocumentMeta { name: "Checkout flow".into(), project_id: Some(WS.into()), can_share: true }));
        // Archived files don't open; missing fields fall back; nonsense is nothing.
        assert_eq!(document(r#"{"name":"x","state":"archived"}"#), None);
        assert_eq!(document(r#"{"project_id":"../x"}"#), Some(DocumentMeta { name: String::new(), project_id: None, can_share: false }));
        assert_eq!(document("<html>"), None);
        let long = format!(r#"{{"name":"{}"}}"#, "n".repeat(500));
        assert_eq!(document(&long).unwrap().name.chars().count(), figcraft_ui_egui::cloud::MAX_NAME_CHARS);
        assert_eq!(project_name(r#"{"name":"Mobile app"}"#).as_deref(), Some("Mobile app"));
        assert_eq!(project_name(r#"{"name":"   "}"#), None);
    }

    #[test]
    fn only_a_websocket_invite_is_joined() {
        assert_eq!(invite(r#"{"invite":"wss://h/ws/v1/collab?file=a&token=b"}"#).as_deref(), Some("wss://h/ws/v1/collab?file=a&token=b"));
        assert_eq!(invite(r#"{"invite":"https://evil/x"}"#), None);
        assert_eq!(invite(r#"{"invite":"ws://h/a\nb"}"#), None);
        assert_eq!(invite(&format!(r#"{{"invite":"wss://{}"}}"#, "a".repeat(5000))), None);
        assert_eq!(invite("{}"), None);
    }

    #[test]
    fn statuses_and_sign_in() {
        assert_eq!(failure(401), Failure::SignIn);
        assert_eq!(failure(403), Failure::NoAccess);
        assert_eq!(failure(404), Failure::NoAccess);
        assert_eq!(failure(500), Failure::Unavailable);
        assert_eq!(failure(0), Failure::Unavailable);
        assert_eq!(sign_in_url(&format!("/workspace/{WS}/file/x")), format!("/api/v1/auth/login?return_to=%2Fworkspace%2F{WS}%2Ffile%2Fx"));
    }
}
