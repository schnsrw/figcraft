//! Images by reference (FP-013; `docs/collab-protocol.md`, "Images by reference"): a large image
//! travels as its content hash plus its pixel size and byte length, and its bytes go to the
//! server's asset store by some other channel (HTTP, in FigCraft Cloud), so a batch stays small.
//!
//! The capability is negotiated like the drag signal: a client announces
//! [`IMAGE_REFS`](crate::message::IMAGE_REFS) in its hello, a server that holds assets answers
//! with an [`Signal::ASSETS`](crate::message::Signal::ASSETS) signal carrying its
//! [`AssetPolicy`], and only then does a client send references. A server without assets never
//! sends the signal (the client keeps sending images inline, within the old limits); a client
//! that did not announce the word is never sent a reference (the server leaves them out of what
//! it sends it).
//!
//! The reference is the value of the same [`Entry::Image`](crate::Entry::Image) entry an inline
//! image uses: `{"width":…,"height":…,"asset":{"len":…}}` instead of `{"width":…,"height":…,
//! "bytes":"<base64>"}`. A replica that applies one holds a placeholder (an [`ImageData`] with no
//! bytes) until the bytes arrive; the bytes are content-addressed, so they are checked against the
//! hash before they are used.

use std::fmt;
use std::sync::Arc;

use figcraft_doc::ImageData;
use serde::{Deserialize, Serialize};

use crate::change::{Change, Entry};
use crate::{Error, docprops};

/// Images from this size (bytes) travel as references when the server allows it.
pub const DEFAULT_THRESHOLD: u64 = 256 * 1024;

/// The largest image a server accepts by default (bytes).
pub const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Pixels per side beyond which a reference is refused.
pub const MAX_SIDE: u32 = 1 << 20;

/// What a server that holds assets tells a client that announced
/// [`IMAGE_REFS`](crate::message::IMAGE_REFS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetPolicy {
    /// Images of at least this many bytes should travel as references.
    pub threshold: u64,
    /// The largest image the server takes (bytes), by reference.
    pub max_bytes: u64,
}

impl Default for AssetPolicy {
    fn default() -> Self {
        AssetPolicy { threshold: DEFAULT_THRESHOLD, max_bytes: DEFAULT_MAX_BYTES }
    }
}

/// Whether the server holds a verified asset: the relay's answer to "may this document refer to
/// it?". `hash` is the lower-case hex SHA-256 of the file and `len` its size in bytes; an asset of
/// another tenant, one that was never verified, or one of another size is not found.
pub trait AssetDirectory: Send + Sync + fmt::Debug {
    fn verified(&self, hash: &str, len: u64) -> bool;
}

/// A reference to an image, as it appears in a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub width: u32,
    pub height: u32,
    /// The file's size in bytes; 0 when a server sends state it holds no size for (its snapshot).
    pub len: u64,
}

/// Whether `s` is a lower-case hex SHA-256 (the only keys that can be references).
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The change that puts image `hash` in the document by reference.
pub fn ref_change(hash: &str, width: u32, height: u32, len: u64) -> Result<Change, Error> {
    Ok(Change::Put { entry: Entry::Image(hash.to_string()), value: docprops::ref_value(width, height, len)? })
}

/// The reference a `Put` of an image carries (`None`: an inline image, or not an image entry).
pub fn image_ref(change: &Change) -> Option<ImageRef> {
    match change {
        Change::Put { entry: Entry::Image(_), value } => docprops::image_ref_of(value),
        _ => None,
    }
}

/// Whether any change of the batch is an image reference.
pub fn has_refs(changes: &[Change]) -> bool {
    changes.iter().any(|c| image_ref(c).is_some())
}

/// The batch without its image references, for a peer that cannot read them.
pub fn without_refs(changes: &[Change]) -> Vec<Change> {
    changes.iter().filter(|c| image_ref(c).is_none()).cloned().collect()
}

/// An image that has no bytes yet.
pub fn is_placeholder(image: &ImageData) -> bool {
    image.bytes.is_empty()
}

/// A placeholder for an image whose bytes have not arrived.
pub fn placeholder(width: u32, height: u32) -> ImageData {
    ImageData { bytes: Arc::new(Vec::new()), width, height }
}

/// An image whose bytes this client lacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingImage {
    pub hash: String,
    pub width: u32,
    pub height: u32,
}
