//! The native link's image transfers (TS-009): a few worker threads that upload images to and
//! fetch them from the asset routes of a FigCraft Cloud server ([`crate::assets`]), retrying for a
//! while on their own and reporting the outcome to the session's inbox as
//! [`Event::Asset`](figcraft_engine::live::Event::Asset). The engine asks, returns at once and
//! hears when each transfer is over.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use figcraft_engine::live::{AssetEvent, Event};

use crate::assets::{self, Outcome, ROOM_TOKEN_HEADER};
use crate::http::{self, Request, Response};
use crate::inbox::Inbox;
use crate::invite::Invite;
use crate::net::NetLimits;

/// Uploads at once.
const UPLOADERS: usize = 2;
/// Downloads at once.
const FETCHERS: usize = 3;
/// How long a connection may take to open.
const CONNECT: Duration = Duration::from_secs(10);
/// How long to wait for the server's answer to a request (it checks and stores an upload first).
const ANSWER: Duration = Duration::from_secs(120);

type Job = Box<dyn FnOnce() + Send>;

/// The pauses between attempts (tests make them short).
#[derive(Clone, Debug)]
pub(crate) struct Pauses {
    pub(crate) upload: Vec<Duration>,
    pub(crate) fetch: Vec<Duration>,
}

impl Default for Pauses {
    fn default() -> Self {
        Self { upload: assets::UPLOAD_PAUSES.to_vec(), fetch: assets::FETCH_PAUSES.to_vec() }
    }
}

/// The worker threads (started on first use; they end when this is dropped).
pub(crate) struct Workers {
    up: Sender<Job>,
    down: Sender<Job>,
}

fn spawn(name: &str, count: usize) -> Option<Sender<Job>> {
    let (tx, rx) = channel::<Job>();
    let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
    for i in 0..count {
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("{name}-{i}"))
            .spawn(move || {
                loop {
                    // The lock is only held while waiting for the next job, not while running it.
                    let job = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => return,
                    }
                }
            })
            .ok()?;
    }
    Some(tx)
}

impl Workers {
    pub(crate) fn start() -> Option<Self> {
        Some(Self { up: spawn("image-upload", UPLOADERS)?, down: spawn("image-fetch", FETCHERS)? })
    }

    /// Upload `bytes` as image `hash` of the file behind `invite`.
    pub(crate) fn put(&self, inbox: Inbox, invite: Invite, hash: String, bytes: Arc<Vec<u8>>, pauses: Vec<Duration>, limits: NetLimits) -> Result<(), String> {
        let job: Job = Box::new(move || {
            let result = transfer(&pauses, "the upload", || {
                let url = assets::image_url(&invite, &hash).ok_or("this server has no place for images")?;
                let headers = [(ROOM_TOKEN_HEADER, invite.token.as_str()), ("Content-Type", "application/octet-stream")];
                http::send(&Request { method: "PUT", url: &url, headers: &headers, body: &bytes }, 64 * 1024, CONNECT, ANSWER, &limits)
            })
            .map(|_| ());
            inbox.push(Event::Asset(AssetEvent::Stored { hash, result }));
        });
        self.up.send(job).map_err(|_| "the image transfers have stopped".to_string())
    }

    /// Fetch image `hash` of the file behind `invite`.
    pub(crate) fn get(&self, inbox: Inbox, invite: Invite, hash: String, pauses: Vec<Duration>, limits: NetLimits) -> Result<(), String> {
        let job: Job = Box::new(move || {
            let result = transfer(&pauses, "the download", || {
                let url = assets::image_url(&invite, &hash).ok_or("this server has no place for images")?;
                let headers = [(ROOM_TOKEN_HEADER, invite.token.as_str())];
                http::send(&Request { method: "GET", url: &url, headers: &headers, body: &[] }, assets::MAX_IMAGE_BYTES, CONNECT, ANSWER, &limits)
            })
            .map(|r| r.body);
            inbox.push(Event::Asset(AssetEvent::Fetched { hash, result }));
        });
        self.down.send(job).map_err(|_| "the image transfers have stopped".to_string())
    }
}

/// Run `once` until it works, is refused for good, or the pauses run out. The error says what
/// went wrong in words for the person.
fn transfer(pauses: &[Duration], what: &str, mut once: impl FnMut() -> Result<Response, String>) -> Result<Response, String> {
    let mut attempt = 0;
    loop {
        let (outcome, failure) = match once() {
            Ok(reply) => match assets::judge(reply.status) {
                Outcome::Done => return Ok(reply),
                other => (other, assets::describe(what, other)),
            },
            // Could not talk to the server at all: try again.
            Err(e) => (Outcome::Retry, format!("{what} failed: {e}")),
        };
        match (outcome, pauses.get(attempt)) {
            (Outcome::Retry, Some(pause)) => {
                std::thread::sleep(*pause);
                attempt += 1;
            }
            _ => return Err(failure),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn reply(status: u16) -> Result<Response, String> {
        Ok(Response { status, body: vec![1, 2, 3] })
    }

    #[test]
    fn it_retries_what_may_work_later_and_stops_at_what_never_will() {
        let pauses = [Duration::from_millis(1); 3];
        let calls = Cell::new(0);
        let sequence = [503u16, 500, 200];
        let ok = transfer(&pauses, "the download", || {
            calls.set(calls.get() + 1);
            reply(sequence[calls.get() - 1])
        });
        assert_eq!((ok.map(|r| r.body), calls.get()), (Ok(vec![1, 2, 3]), 3));

        for status in [404, 413, 401, 403, 415, 507] {
            let calls = Cell::new(0);
            let err = transfer(&pauses, "the upload", || {
                calls.set(calls.get() + 1);
                reply(status)
            });
            assert!(err.is_err() && calls.get() == 1, "{status}: refused at once");
        }

        let calls = Cell::new(0);
        let err = transfer(&pauses, "the upload", || {
            calls.set(calls.get() + 1);
            Err("no answer".into())
        })
        .unwrap_err();
        assert_eq!(calls.get(), 4, "the first attempt and one per pause");
        assert!(err.contains("no answer"), "{err}");

        let calls = Cell::new(0);
        let err = transfer(&pauses, "the download", || {
            calls.set(calls.get() + 1);
            reply(503)
        })
        .unwrap_err();
        assert_eq!(calls.get(), 4);
        assert!(err.contains("busy or unreachable"), "{err}");
    }
}
