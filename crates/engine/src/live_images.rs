//! Large images in live sessions (TS-009; FP-013, `docs/collab-protocol.md` "Images by
//! reference"): a session whose server holds assets tells the client its [`AssetPolicy`]; from
//! then on an image the user adds that is at least the policy's threshold is uploaded to the
//! server's asset store by the link ([`LiveLink::asset_put`]) before the batch that adds it goes
//! out (the batch is made at once and waits in the sync client), and the images other people add
//! arrive as placeholders that the link fetches by hash ([`LiveLink::asset_get`]), verified
//! against their hash before they are shown. Servers that hold no assets, and links that cannot
//! move assets, keep today's behaviour: images travel inline.
//!
//! This is a child module of `live` for its private state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use figcraft_collab::AssetPolicy;
use figcraft_doc::Document;

use super::{Lives, Session, SessionId};

/// The largest image sent inside a batch when the server takes no references (a 64 MiB message
/// holds 48 MiB of it as base64; this leaves room for the rest of the batch).
pub const MAX_INLINE_IMAGE: usize = 40 << 20;

/// What a link reports about an image transfer it was asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetEvent {
    /// The upload of image `hash` finished (after the link's own retries) or failed for good.
    Stored { hash: String, result: Result<(), String> },
    /// The download of image `hash` finished (after the link's own retries) or failed for good.
    Fetched { hash: String, result: Result<Vec<u8>, String> },
}

/// Image transfers in a session now.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImageActivity {
    /// Uploads under way.
    pub uploading: usize,
    /// Downloads under way.
    pub fetching: usize,
    /// Images that could not be fetched (tried again at the next reconnect).
    pub failed: usize,
}

/// A session's image state.
#[derive(Default)]
pub(super) struct Images {
    /// What the server said it takes (`None`: images stay inline).
    pub(super) policy: Option<AssetPolicy>,
    uploading: BTreeMap<String, u64>,
    fetching: BTreeSet<String>,
    failed: BTreeMap<String, String>,
}

impl Images {
    /// A new connection: the server says again what it takes, and images that failed are tried
    /// again.
    pub(super) fn reconnecting(&mut self) {
        self.policy = None;
        self.failed.clear();
    }
}

impl Lives {
    fn asset_put(&mut self, id: SessionId, hash: &str, bytes: Arc<Vec<u8>>) -> Result<(), String> {
        match self.link.as_mut() {
            Some(link) => link.asset_put(id, hash, bytes),
            None => Err("there is no connection".into()),
        }
    }

    fn asset_get(&mut self, id: SessionId, hash: &str) -> Result<(), String> {
        match self.link.as_mut() {
            Some(link) => link.asset_get(id, hash),
            None => Err("there is no connection".into()),
        }
    }
}

impl Session {
    /// Image transfers in the active document's session.
    pub fn live_image_activity(&self) -> ImageActivity {
        let live = self.live();
        ImageActivity { uploading: live.images.uploading.len(), fetching: live.images.fetching.len(), failed: live.images.failed.len() }
    }

    /// Before `doc` (the session document with its new edits) is synced: start uploading the
    /// large images it adds, so the batch that adds them goes out as references once they are
    /// there. An `Err` says why the edit can't be synced at all (an image the server can't take
    /// and a message can't carry); the caller puts the document back.
    pub(super) fn live_images_begin(&mut self, id: SessionId, doc: &Document) -> Result<(), String> {
        let todo: Vec<(String, Arc<Vec<u8>>)> = {
            let Some(live) = self.live.sessions.get_mut(&id) else { return Ok(()) };
            let Some(client) = live.client.as_mut() else { return Ok(()) };
            let Some(policy) = live.images.policy else {
                // Inline: refuse what no message can carry (and the transport would drop the
                // connection over, for ever).
                let have = &client.document().images;
                for (hash, image) in &doc.images {
                    if !have.contains_key(hash) && image.bytes.len() > MAX_INLINE_IMAGE {
                        return Err(format!(
                            "That image is {} MB, more than this session can carry ({} MB). Use a smaller image",
                            image.bytes.len() >> 20,
                            MAX_INLINE_IMAGE >> 20
                        ));
                    }
                }
                return Ok(());
            };
            let todo = client.images_to_upload(doc, policy.threshold);
            if let Some((_, bytes)) = todo.iter().find(|(_, bytes)| u64::try_from(bytes.len()).map_or(true, |n| n > policy.max_bytes)) {
                return Err(format!(
                    "That image is {} MB, more than the server takes ({} MB). Use a smaller image",
                    bytes.len() >> 20,
                    policy.max_bytes >> 20
                ));
            }
            for (hash, bytes) in &todo {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                client.expect_upload(hash, len);
                live.images.uploading.insert(hash.clone(), len);
            }
            todo
        };
        for (hash, bytes) in todo {
            if let Err(why) = self.live.asset_put(id, &hash, bytes) {
                self.live_image_stored(id, hash, Err(why));
            }
        }
        Ok(())
    }

    /// An upload finished: the batches that waited for it go out, or (it failed for good) they
    /// go out with the bytes inline and the server decides.
    fn live_image_stored(&mut self, id: SessionId, hash: String, result: Result<(), String>) {
        let Some(live) = self.live.sessions.get_mut(&id) else { return };
        let len = live.images.uploading.remove(&hash).unwrap_or(0);
        let Some(client) = live.client.as_mut() else { return };
        let (out, note) = match result {
            Ok(()) => (client.mark_uploaded(&hash, len), None),
            Err(why) => (client.abandon_upload(&hash), Some(format!("Couldn't upload an image ({why}); sending it in the file instead"))),
        };
        self.live.send(id, out);
        if note.is_some() {
            self.message = note;
        }
    }

    /// Ask the link for the images session `id` has only as placeholders.
    pub(super) fn live_images_fetch(&mut self, id: SessionId) {
        let wanted: Vec<String> = {
            let Some(live) = self.live.sessions.get_mut(&id) else { return };
            let Some(client) = live.client.as_ref() else { return };
            let wanted: Vec<String> = client
                .missing_images()
                .into_iter()
                .map(|m| m.hash)
                .filter(|h| !live.images.fetching.contains(h) && !live.images.failed.contains_key(h))
                .collect();
            live.images.fetching.extend(wanted.iter().cloned());
            wanted
        };
        for hash in wanted {
            if let Err(why) = self.live.asset_get(id, &hash) {
                if let Some(live) = self.live.sessions.get_mut(&id) {
                    live.images.fetching.remove(&hash);
                    live.images.failed.insert(hash, why);
                }
            }
        }
    }

    /// The link finished moving an image.
    pub(super) fn live_asset_event(&mut self, id: SessionId, event: AssetEvent) {
        match event {
            AssetEvent::Stored { hash, result } => self.live_image_stored(id, hash, result),
            AssetEvent::Fetched { hash, result } => {
                let Some(live) = self.live.sessions.get_mut(&id) else { return };
                live.images.fetching.remove(&hash);
                match result {
                    Ok(bytes) => self.live_image_arrived(id, &hash, bytes),
                    Err(why) => {
                        live.images.failed.insert(hash, why.clone());
                        self.message = Some(format!("Couldn't load an image: {why}"));
                    }
                }
            }
        }
    }

    /// Image `hash` arrived: it is shown once it is checked to be the file the hash names.
    fn live_image_arrived(&mut self, id: SessionId, hash: &str, bytes: Vec<u8>) {
        let Some(live) = self.live.sessions.get_mut(&id) else { return };
        if figcraft_format::image_hash(&bytes) != hash {
            live.images.failed.insert(hash.to_string(), "the file does not match its address".into());
            self.message = Some("Couldn't load an image: what the server sent is not the image".into());
            return;
        }
        let Some(client) = live.client.as_mut() else { return };
        if !client.set_image_bytes(hash, Arc::new(bytes)) {
            return;
        }
        let Some(image) = client.document().images.get(hash).cloned() else { return };
        let uid = live.doc_uid;
        if let Some(ds) = self.docs.iter_mut().find(|d| Some(d.uid) == uid) {
            // Edits not sent yet stay unsent: only a document that matched what was synced still does.
            let in_step = live.synced.as_ref().is_some_and(|s| Arc::ptr_eq(s, &ds.doc));
            Arc::make_mut(&mut ds.doc).images.insert(hash.to_string(), image);
            if in_step {
                live.synced = Some(ds.doc.clone());
            }
            ds.revision += 1;
        }
    }
}
