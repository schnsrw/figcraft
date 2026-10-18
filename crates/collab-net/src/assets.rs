//! Moving the bytes of large images (TS-009; FP-013): the HTTP side of "images by reference"
//! (`docs/collab-protocol.md`). A session whose server holds assets keeps image bytes out of the
//! collaboration messages: the editor uploads an image with `PUT` before the batch that adds it,
//! and fetches the images it lacks with `GET`, both at `<api>/documents/<file>/images/<sha256>`,
//! authorised by the room's own token in `X-Figcraft-Room-Token` (and, in a browser, by the
//! session cookie).
//!
//! This file is what the native client ([`crate::http`] over a worker thread) and the browser
//! client (`fetch`) share: where the route is, what a status means, and when to try again.

use std::time::Duration;

use crate::invite::Invite;

/// The header carrying the room's token.
pub const ROOM_TOKEN_HEADER: &str = "X-Figcraft-Room-Token";

/// The path the FigCraft Cloud relay answers on; the API is beside it at `/api/v1`.
pub const RELAY_PATH: &str = "/ws/v1/collab";

/// Largest image a client will take from a server (bytes): the server's own upload limit tops out
/// at 256 MiB.
pub const MAX_IMAGE_BYTES: usize = 256 << 20;

/// How long to wait before each further attempt of an upload (the first is at once).
pub const UPLOAD_PAUSES: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(3), Duration::from_secs(9)];

/// How long to wait before each further attempt of a download: a placeholder is on screen until
/// it works, so it keeps trying for a few minutes.
pub const FETCH_PAUSES: [Duration; 9] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(60),
];

/// The URL of image `hash` of the file behind `invite`, when the server is a FigCraft Cloud one:
/// `http(s)://host/<prefix>/api/v1/documents/<file>/images/<hash>` where `<prefix>` is whatever
/// precedes `/ws/v1/collab` in the relay's address. `None` for any other server.
pub fn image_url(invite: &Invite, hash: &str) -> Option<String> {
    let path = invite.server.path.trim_end_matches('/');
    let prefix = path.strip_suffix(RELAY_PATH)?;
    if invite.file.is_empty() || !invite.file.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !is_hash(hash) {
        return None;
    }
    let scheme = if invite.server.secure { "https" } else { "http" };
    Some(format!("{scheme}://{}{prefix}/api/v1/documents/{}/images/{hash}", invite.server.host, invite.file))
}

fn is_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What a status means for an upload or download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// It worked.
    Done,
    /// Try again later: the server is busy or unreachable.
    Retry,
    /// It will not work however often it is tried; the text says why.
    Refused(&'static str),
}

/// What the HTTP status of a reply means.
pub fn judge(status: u16) -> Outcome {
    match status {
        200 | 201 => Outcome::Done,
        408 | 425 | 429 | 500 | 502 | 503 | 504 => Outcome::Retry,
        400 => Outcome::Refused("the server did not accept it as an image"),
        401 => Outcome::Refused("you are not signed in to this file any more"),
        403 => Outcome::Refused("you can only view this file"),
        404 => Outcome::Refused("the server does not have it"),
        409 => Outcome::Refused("the file is not open for changes"),
        413 => Outcome::Refused("it is larger than the server accepts"),
        415 => Outcome::Refused("the server only takes PNG, JPEG, WebP and GIF images"),
        507 => Outcome::Refused("the workspace has used its image storage"),
        s if (500..600).contains(&s) => Outcome::Retry,
        _ => Outcome::Refused("the server answered something unexpected"),
    }
}

/// A transport failure is worth another try; so are the statuses [`judge`] says to retry.
pub fn describe(what: &str, outcome: Outcome) -> String {
    match outcome {
        Outcome::Done => format!("{what} worked"),
        Outcome::Retry => format!("{what}: the server is busy or unreachable"),
        Outcome::Refused(why) => format!("{what}: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invite::Endpoint;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn invite(path: &str, secure: bool, file: &str) -> Invite {
        Invite { server: Endpoint { host: "cloud.example:8080".into(), path: path.into(), secure }, file: file.into(), token: "t".into() }
    }

    #[test]
    fn the_api_is_beside_the_relay() {
        let doc = "d0c10000-0000-4000-8000-000000000001";
        assert_eq!(
            image_url(&invite("/ws/v1/collab", true, doc), HASH).as_deref(),
            Some(format!("https://cloud.example:8080/api/v1/documents/{doc}/images/{HASH}").as_str())
        );
        assert_eq!(
            image_url(&invite("/ws/v1/collab", false, doc), HASH).as_deref(),
            Some(format!("http://cloud.example:8080/api/v1/documents/{doc}/images/{HASH}").as_str())
        );
        // Behind a path prefix, and with a trailing slash.
        assert_eq!(
            image_url(&invite("/figcraft/ws/v1/collab/", true, doc), HASH).as_deref(),
            Some(format!("https://cloud.example:8080/figcraft/api/v1/documents/{doc}/images/{HASH}").as_str())
        );
    }

    #[test]
    fn only_a_cloud_server_with_sane_names_has_a_url() {
        let doc = "d0c10000-0000-4000-8000-000000000001";
        // Another kind of server: no asset route.
        assert!(image_url(&invite("/", false, doc), HASH).is_none());
        assert!(image_url(&invite("/session", false, doc), HASH).is_none());
        // Nothing that could change the path.
        for bad in ["", "../x", "a/b", "a b", "a?b", "a#b", "a%2f"] {
            assert!(image_url(&invite("/ws/v1/collab", false, bad), HASH).is_none(), "{bad:?}");
        }
        for bad in ["", "abc", &HASH.to_uppercase(), &format!("{HASH}0"), "../../x"] {
            assert!(image_url(&invite("/ws/v1/collab", false, doc), bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn statuses_are_judged() {
        assert_eq!([judge(200), judge(201)], [Outcome::Done; 2]);
        for retry in [408, 429, 500, 502, 503, 504, 599] {
            assert_eq!(judge(retry), Outcome::Retry, "{retry}");
        }
        for refused in [400, 401, 403, 404, 409, 413, 415, 507, 302, 418] {
            assert!(matches!(judge(refused), Outcome::Refused(_)), "{refused}");
        }
        assert!(describe("Uploading an image", judge(413)).contains("larger than the server accepts"));
    }
}
