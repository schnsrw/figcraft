//! Bounded validation of uploaded bytes before an object is promoted out of staging.
//!
//! [`validate`] checks one upload against its declared media type and a set of [`Budgets`]. It
//! reads headers and container directories only: it never decodes pixels, never decompresses and
//! never allocates more than a small multiple of the archive's entry count, so a hostile upload
//! costs at most one linear pass over its bytes. Every refusal is a fixed [`Rejection`] code that
//! is safe to return to the client and record in audit metadata.
//!
//! Accepted media:
//!
//! - `image/png`, `image/jpeg`, `image/webp`, `image/gif`: the magic bytes must match, and the
//!   header (PNG `IHDR` with its CRC, the first JPEG `SOFn`, the WebP `VP8 `/`VP8L`/`VP8X` frame
//!   header, the GIF logical screen) must give non-zero dimensions within the pixel, dimension and
//!   decoded-byte budgets.
//! - `application/x-figcraft`: a `.figcraft` zip (documents and published libraries). Only the
//!   end-of-central-directory record, the central directory and each local header are read;
//!   entry count, names, per-entry size, compression ratio and total declared expanded size are
//!   budgeted, and entries may not overlap, be encrypted, use zip64 or anything but stored or
//!   deflate. The archive must contain `document.json`.
//! - `application/x-figcraft-plugin`: a plugin bundle (`FCPLUGB1`, see `figcraft-plugin`), which
//!   is uncompressed; its manifest, WebAssembly module and assets are bounded and named safely.
//!
//! SVG and PDF are refused: both are active, scriptable formats whose safe acceptance needs a full
//! parser and sanitizer rather than a header check (see `docs/saas/security.md`).
//!
//! Readers must still enforce these limits while decoding: a declared size is what the archive
//! claims, and a decoder that writes past it is the second line of defence.

use std::collections::BTreeSet;

/// PNG image.
pub const MEDIA_PNG: &str = "image/png";
/// JPEG image.
pub const MEDIA_JPEG: &str = "image/jpeg";
/// WebP image.
pub const MEDIA_WEBP: &str = "image/webp";
/// GIF image.
pub const MEDIA_GIF: &str = "image/gif";
/// A `.figcraft` document or published library (a zip container).
pub const MEDIA_FIGCRAFT: &str = "application/x-figcraft";
/// A FigCraft plugin bundle.
pub const MEDIA_PLUGIN: &str = "application/x-figcraft-plugin";

/// A media type this validator accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaType {
    Png,
    Jpeg,
    Webp,
    Gif,
    Figcraft,
    PluginBundle,
}

impl MediaType {
    pub const ALL: [Self; 6] = [Self::Png, Self::Jpeg, Self::Webp, Self::Gif, Self::Figcraft, Self::PluginBundle];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => MEDIA_PNG,
            Self::Jpeg => MEDIA_JPEG,
            Self::Webp => MEDIA_WEBP,
            Self::Gif => MEDIA_GIF,
            Self::Figcraft => MEDIA_FIGCRAFT,
            Self::PluginBundle => MEDIA_PLUGIN,
        }
    }

    /// The accepted media type a client declared, compared without ASCII case. Parameters
    /// (`; charset=…`), surrounding spaces and every other type are refused.
    #[must_use]
    pub fn from_declared(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|media| media.as_str().eq_ignore_ascii_case(value))
    }

    /// The media type the leading magic bytes identify, if any.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(PNG_SIGNATURE) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
            Some(Self::Webp)
        } else if bytes.starts_with(&LOCAL_SIG.to_le_bytes()) {
            Some(Self::Figcraft)
        } else if bytes.starts_with(PLUGIN_MAGIC) {
            Some(Self::PluginBundle)
        } else {
            None
        }
    }
}

/// Limits applied by [`validate`]. [`Budgets::DEFAULT`] matches the readers in `figcraft-format`
/// and `figcraft-plugin`; callers may only tighten them per tenant plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budgets {
    /// Largest upload accepted at all.
    pub max_input_bytes: u64,
    /// Most pixels (width × height) in an image.
    pub max_pixels: u64,
    /// Largest width or height of an image.
    pub max_dimension: u32,
    /// Most bytes an image may need once decoded (pixels × 4, or × 8 for 16-bit samples).
    pub max_decoded_bytes: u64,
    /// Most entries in an archive, or assets in a plugin bundle.
    pub max_entries: u32,
    /// Largest declared uncompressed entry (a plugin module or asset counts as one).
    pub max_entry_bytes: u64,
    /// Most declared uncompressed bytes across an archive.
    pub max_expanded_bytes: u64,
    /// Largest uncompressed ÷ compressed ratio of an entry above [`Budgets::ratio_floor_bytes`].
    pub max_ratio: u64,
    /// Entries expanding to at most this many bytes are not ratio-checked.
    pub ratio_floor_bytes: u64,
    /// Longest entry name in bytes.
    pub max_name_bytes: usize,
}

impl Budgets {
    pub const DEFAULT: Self = Self {
        max_input_bytes: 512 * MIB,
        max_pixels: 100_000_000,
        max_dimension: 65_535,
        max_decoded_bytes: 512 * MIB,
        max_entries: 50_000,
        max_entry_bytes: 512 * MIB,
        max_expanded_bytes: 2048 * MIB,
        max_ratio: 250,
        ratio_floor_bytes: MIB,
        max_name_bytes: 255,
    };
}

impl Default for Budgets {
    fn default() -> Self {
        Self::DEFAULT
    }
}

const MIB: u64 = 1024 * 1024;

/// What a successful validation established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Validated {
    pub media: MediaType,
    pub summary: Summary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Summary {
    /// An image's header dimensions and the bytes it needs decoded.
    Image { width: u32, height: u32, decoded_bytes: u64 },
    /// A zip container's entry count and total declared uncompressed bytes.
    Archive { entries: u32, expanded_bytes: u64 },
    /// A plugin bundle's asset count and module size.
    Plugin { assets: u32, module_bytes: u64 },
}

/// Why an upload was refused. Codes are fixed and never contain input bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Empty,
    InputTooLarge,
    UnsupportedMediaType,
    MediaMismatch,
    Truncated,
    Malformed,
    HeaderChecksum,
    TrailingBytes,
    ZeroDimension,
    DimensionTooLarge,
    PixelBudget,
    DecodedByteBudget,
    EntryCountBudget,
    EntrySizeBudget,
    ExpandedByteBudget,
    CompressionRatio,
    UnsupportedCompression,
    Encrypted,
    Zip64Unsupported,
    OverlappingEntries,
    UnsafeName,
    DuplicateName,
    MissingMember,
}

impl Rejection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::InputTooLarge => "input_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::MediaMismatch => "media_mismatch",
            Self::Truncated => "truncated",
            Self::Malformed => "malformed",
            Self::HeaderChecksum => "header_checksum",
            Self::TrailingBytes => "trailing_bytes",
            Self::ZeroDimension => "zero_dimension",
            Self::DimensionTooLarge => "dimension_too_large",
            Self::PixelBudget => "pixel_budget",
            Self::DecodedByteBudget => "decoded_byte_budget",
            Self::EntryCountBudget => "entry_count_budget",
            Self::EntrySizeBudget => "entry_size_budget",
            Self::ExpandedByteBudget => "expanded_byte_budget",
            Self::CompressionRatio => "compression_ratio",
            Self::UnsupportedCompression => "unsupported_compression",
            Self::Encrypted => "encrypted",
            Self::Zip64Unsupported => "zip64_unsupported",
            Self::OverlappingEntries => "overlapping_entries",
            Self::UnsafeName => "unsafe_name",
            Self::DuplicateName => "duplicate_name",
            Self::MissingMember => "missing_member",
        }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Rejection {}

/// Validate `bytes` declared as `declared_media_type` against `budgets`.
pub fn validate(declared_media_type: &str, bytes: &[u8], budgets: &Budgets) -> Result<Validated, Rejection> {
    if bytes.is_empty() {
        return Err(Rejection::Empty);
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > budgets.max_input_bytes {
        return Err(Rejection::InputTooLarge);
    }
    let media = MediaType::from_declared(declared_media_type).ok_or(Rejection::UnsupportedMediaType)?;
    if MediaType::sniff(bytes) != Some(media) {
        return Err(Rejection::MediaMismatch);
    }
    let summary = match media {
        MediaType::Png => png(bytes, budgets)?,
        MediaType::Jpeg => jpeg(bytes, budgets)?,
        MediaType::Webp => webp(bytes, budgets)?,
        MediaType::Gif => gif(bytes, budgets)?,
        MediaType::Figcraft => figcraft(bytes, budgets)?,
        MediaType::PluginBundle => plugin(bytes, budgets)?,
    };
    Ok(Validated { media, summary })
}

// ---- byte access: every read is bounds-checked and a short read is `Truncated` ----

fn slice(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], Rejection> {
    let end = at.checked_add(len).ok_or(Rejection::Truncated)?;
    bytes.get(at..end).ok_or(Rejection::Truncated)
}

fn byte(bytes: &[u8], at: usize) -> Result<u8, Rejection> {
    bytes.get(at).copied().ok_or(Rejection::Truncated)
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], Rejection> {
    slice(bytes, at, N)?.try_into().map_err(|_| Rejection::Truncated)
}

fn le16(bytes: &[u8], at: usize) -> Result<u16, Rejection> {
    array(bytes, at).map(u16::from_le_bytes)
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, Rejection> {
    array(bytes, at).map(u32::from_le_bytes)
}

fn be16(bytes: &[u8], at: usize) -> Result<u16, Rejection> {
    array(bytes, at).map(u16::from_be_bytes)
}

fn be32(bytes: &[u8], at: usize) -> Result<u32, Rejection> {
    array(bytes, at).map(u32::from_be_bytes)
}

fn le24(bytes: &[u8], at: usize) -> Result<u32, Rejection> {
    let [a, b, c] = array::<3>(bytes, at)?;
    Ok(u32::from_le_bytes([a, b, c, 0]))
}

fn to_usize(value: u32) -> Result<usize, Rejection> {
    usize::try_from(value).map_err(|_| Rejection::Malformed)
}

fn add(a: usize, b: usize) -> Result<usize, Rejection> {
    a.checked_add(b).ok_or(Rejection::Malformed)
}

// ---- images ----

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// Most JPEG segments walked before the first frame header.
const MAX_JPEG_SEGMENTS: usize = 1024;

fn image_budget(width: u32, height: u32, bytes_per_pixel: u64, budgets: &Budgets) -> Result<Summary, Rejection> {
    if width == 0 || height == 0 {
        return Err(Rejection::ZeroDimension);
    }
    if width > budgets.max_dimension || height > budgets.max_dimension {
        return Err(Rejection::DimensionTooLarge);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > budgets.max_pixels {
        return Err(Rejection::PixelBudget);
    }
    let decoded_bytes = pixels.checked_mul(bytes_per_pixel).ok_or(Rejection::DecodedByteBudget)?;
    if decoded_bytes > budgets.max_decoded_bytes {
        return Err(Rejection::DecodedByteBudget);
    }
    Ok(Summary::Image { width, height, decoded_bytes })
}

/// The `IHDR` chunk, which the PNG specification requires first; its CRC must match.
fn png(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let length = be32(bytes, 8)?;
    if length != 13 || slice(bytes, 12, 4)? != b"IHDR" {
        return Err(Rejection::Malformed);
    }
    let crc = be32(bytes, 29)?;
    if crc32fast::hash(slice(bytes, 12, 17)?) != crc {
        return Err(Rejection::HeaderChecksum);
    }
    let width = be32(bytes, 16)?;
    let height = be32(bytes, 20)?;
    let [depth, color, compression, filter, interlace] = array::<5>(bytes, 24)?;
    let valid_depth = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    // The specification limits each dimension to 2^31 - 1.
    if !valid_depth || compression != 0 || filter != 0 || interlace > 1 || width > 0x7FFF_FFFF || height > 0x7FFF_FFFF {
        return Err(Rejection::Malformed);
    }
    image_budget(width, height, if depth == 16 { 8 } else { 4 }, budgets)
}

/// Walk marker segments to the first start-of-frame (`SOF0`–`SOF15` except `DHT`, `JPG`, `DAC`).
fn jpeg(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let mut at = 2;
    for _ in 0..MAX_JPEG_SEGMENTS {
        if byte(bytes, at)? != 0xFF {
            return Err(Rejection::Malformed);
        }
        while byte(bytes, at)? == 0xFF {
            at = add(at, 1)?;
        }
        let marker = byte(bytes, at)?;
        at = add(at, 1)?;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => continue,
            // A second image start, end of image or scan before any frame header.
            0x00 | 0xD8 | 0xD9 | 0xDA => return Err(Rejection::Malformed),
            _ => {}
        }
        let length = usize::from(be16(bytes, at)?);
        if length < 2 {
            return Err(Rejection::Malformed);
        }
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if length < 8 {
                return Err(Rejection::Malformed);
            }
            let precision = byte(bytes, add(at, 2)?)?;
            let height = be16(bytes, add(at, 3)?)?;
            let width = be16(bytes, add(at, 5)?)?;
            let components = byte(bytes, add(at, 7)?)?;
            if !(2..=16).contains(&precision) || !(1..=4).contains(&components) {
                return Err(Rejection::Malformed);
            }
            // A zero height defers to a later DNL segment, which is not supported.
            return image_budget(u32::from(width), u32::from(height), if precision > 8 { 8 } else { 4 }, budgets);
        }
        at = add(at, length)?;
    }
    Err(Rejection::Malformed)
}

/// The RIFF container must span the upload exactly; the first chunk is the frame header.
fn webp(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let riff = to_usize(le32(bytes, 4)?)?;
    if riff < 4 || riff % 2 != 0 {
        return Err(Rejection::Malformed);
    }
    let total = add(riff, 8)?;
    if total > bytes.len() {
        return Err(Rejection::Truncated);
    }
    if total < bytes.len() {
        return Err(Rejection::TrailingBytes);
    }
    let fourcc = array::<4>(bytes, 12)?;
    let chunk = to_usize(le32(bytes, 16)?)?;
    if add(add(chunk, 20)?, chunk % 2)? > total {
        return Err(Rejection::Truncated);
    }
    let data = slice(bytes, 20, chunk)?;
    let (width, height) = match &fourcc {
        b"VP8 " => {
            // A key frame: the frame tag's low bit is clear, then the start code.
            if byte(data, 0)? & 1 != 0 || slice(data, 3, 3)? != [0x9D, 0x01, 0x2A] {
                return Err(Rejection::Malformed);
            }
            (u32::from(le16(data, 6)? & 0x3FFF), u32::from(le16(data, 8)? & 0x3FFF))
        }
        b"VP8L" => {
            let bits = le32(data, 1)?;
            if byte(data, 0)? != 0x2F || bits >> 29 != 0 {
                return Err(Rejection::Malformed);
            }
            ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => {
            if chunk < 10 {
                return Err(Rejection::Malformed);
            }
            (le24(data, 4)? + 1, le24(data, 7)? + 1)
        }
        _ => return Err(Rejection::Malformed),
    };
    image_budget(width, height, 4, budgets)
}

/// The logical screen descriptor after the six-byte signature.
fn gif(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let width = le16(bytes, 6)?;
    let height = le16(bytes, 8)?;
    slice(bytes, 10, 3)?;
    image_budget(u32::from(width), u32::from(height), 4, budgets)
}

// ---- zip containers ----

const LOCAL_SIG: u32 = 0x0403_4B50;
const CENTRAL_SIG: u32 = 0x0201_4B50;
const EOCD_SIG: u32 = 0x0605_4B50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4B50;
const EOCD_LEN: usize = 22;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;
const MAX_COMMENT: usize = 0xFFFF;
/// General-purpose flags: traditional and strong encryption.
const FLAG_ENCRYPTED: u16 = 0x0001 | 0x0040;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const METHOD_AES: u16 = 99;
/// The member a `.figcraft` archive must contain.
const FIGCRAFT_DOCUMENT: &str = "document.json";

/// The end-of-central-directory record: the last signature whose comment ends the upload exactly.
fn find_eocd(bytes: &[u8]) -> Result<usize, Rejection> {
    let last = bytes.len().checked_sub(EOCD_LEN).ok_or(Rejection::Truncated)?;
    let first = last.saturating_sub(MAX_COMMENT);
    for at in (first..=last).rev() {
        if le32(bytes, at)? == EOCD_SIG && add(add(at, EOCD_LEN)?, usize::from(le16(bytes, add(at, 20)?)?))? == bytes.len() {
            return Ok(at);
        }
    }
    Err(Rejection::Malformed)
}

fn figcraft(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let (entries, expanded_bytes, has_document) = zip_directory(bytes, budgets)?;
    if !has_document {
        return Err(Rejection::MissingMember);
    }
    Ok(Summary::Archive { entries, expanded_bytes })
}

/// Walk the central directory and each entry's local header (never its data). Returns the entry
/// count, the total declared uncompressed size and whether `document.json` is present.
fn zip_directory(bytes: &[u8], budgets: &Budgets) -> Result<(u32, u64, bool), Rejection> {
    let eocd = find_eocd(bytes)?;
    if eocd >= 20 && le32(bytes, eocd - 20)? == ZIP64_LOCATOR_SIG {
        return Err(Rejection::Zip64Unsupported);
    }
    let disk = le16(bytes, eocd + 4)?;
    let directory_disk = le16(bytes, eocd + 6)?;
    let disk_entries = le16(bytes, eocd + 8)?;
    let entries = le16(bytes, eocd + 10)?;
    let directory_size = le32(bytes, eocd + 12)?;
    let directory_offset = le32(bytes, eocd + 16)?;
    if entries == u16::MAX || directory_size == u32::MAX || directory_offset == u32::MAX {
        return Err(Rejection::Zip64Unsupported);
    }
    if disk != 0 || directory_disk != 0 || disk_entries != entries {
        return Err(Rejection::Malformed);
    }
    if u32::from(entries) > budgets.max_entries {
        return Err(Rejection::EntryCountBudget);
    }
    let directory_start = to_usize(directory_offset)?;
    let directory_end = add(directory_start, to_usize(directory_size)?)?;
    // The directory ends where the record starts: nothing hides between them.
    if directory_end != eocd {
        return Err(Rejection::Malformed);
    }
    let directory = bytes.get(..directory_end).ok_or(Rejection::Malformed)?;

    let mut names = BTreeSet::new();
    let mut ranges = Vec::with_capacity(usize::from(entries));
    let mut expanded: u64 = 0;
    let mut has_document = false;
    let mut at = directory_start;
    for _ in 0..entries {
        if le32(directory, at)? != CENTRAL_SIG {
            return Err(Rejection::Malformed);
        }
        let flags = le16(directory, at + 8)?;
        let method = le16(directory, at + 10)?;
        let compressed = le32(directory, at + 20)?;
        let uncompressed = le32(directory, at + 24)?;
        let name_len = usize::from(le16(directory, at + 28)?);
        let extra_len = usize::from(le16(directory, at + 30)?);
        let comment_len = usize::from(le16(directory, at + 32)?);
        let start_disk = le16(directory, at + 34)?;
        let local_offset = le32(directory, at + 42)?;
        let name = slice(directory, add(at, CENTRAL_LEN)?, name_len)?;
        at = add(add(add(add(at, CENTRAL_LEN)?, name_len)?, extra_len)?, comment_len)?;
        if at > directory_end {
            return Err(Rejection::Malformed);
        }
        if flags & FLAG_ENCRYPTED != 0 || method == METHOD_AES {
            return Err(Rejection::Encrypted);
        }
        if compressed == u32::MAX || uncompressed == u32::MAX || local_offset == u32::MAX {
            return Err(Rejection::Zip64Unsupported);
        }
        if method != METHOD_STORED && method != METHOD_DEFLATE {
            return Err(Rejection::UnsupportedCompression);
        }
        if start_disk != 0 || (method == METHOD_STORED && compressed != uncompressed) {
            return Err(Rejection::Malformed);
        }
        let directory_entry = check_name(name, budgets)?;
        if directory_entry && uncompressed != 0 {
            return Err(Rejection::Malformed);
        }
        // Names that differ only by ASCII case collide on case-insensitive file systems.
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(Rejection::DuplicateName);
        }
        has_document |= name == FIGCRAFT_DOCUMENT.as_bytes();

        let size = u64::from(uncompressed);
        if size > budgets.max_entry_bytes {
            return Err(Rejection::EntrySizeBudget);
        }
        if size > budgets.ratio_floor_bytes && size > u64::from(compressed).saturating_mul(budgets.max_ratio) {
            return Err(Rejection::CompressionRatio);
        }
        expanded = expanded.checked_add(size).ok_or(Rejection::ExpandedByteBudget)?;
        if expanded > budgets.max_expanded_bytes {
            return Err(Rejection::ExpandedByteBudget);
        }

        // The local header must agree with the directory, and its data must end before the
        // directory starts.
        let local = to_usize(local_offset)?;
        if le32(directory, local)? != LOCAL_SIG || le16(directory, add(local, 8)?)? != method {
            return Err(Rejection::Malformed);
        }
        let local_name_len = usize::from(le16(directory, add(local, 26)?)?);
        let local_extra_len = usize::from(le16(directory, add(local, 28)?)?);
        if slice(directory, add(local, LOCAL_LEN)?, local_name_len)? != name {
            return Err(Rejection::Malformed);
        }
        let data_end = add(add(add(local, LOCAL_LEN)?, local_name_len + local_extra_len)?, to_usize(compressed)?)?;
        if data_end > directory_start {
            return Err(Rejection::Malformed);
        }
        ranges.push((local, data_end));
    }
    if at != directory_end {
        return Err(Rejection::Malformed);
    }
    // Entries are disjoint and the first starts the upload, so no bytes are shared between
    // entries (overlap bombs) or hidden in front of them (polyglots).
    ranges.sort_unstable();
    let mut end = 0;
    for (index, (start, stop)) in ranges.iter().enumerate() {
        if index == 0 && *start != 0 {
            return Err(Rejection::Malformed);
        }
        if *start < end {
            return Err(Rejection::OverlappingEntries);
        }
        end = *stop;
    }
    Ok((u32::from(entries), expanded, has_document))
}

/// A safe relative entry name: UTF-8, no control characters, `\` or `:`, not absolute, and no
/// empty, `.` or `..` segment. A single trailing `/` marks a directory entry (returned as true).
fn check_name(name: &[u8], budgets: &Budgets) -> Result<bool, Rejection> {
    if name.is_empty() || name.len() > budgets.max_name_bytes {
        return Err(Rejection::UnsafeName);
    }
    let text = std::str::from_utf8(name).map_err(|_| Rejection::UnsafeName)?;
    if text.chars().any(|c| c.is_control() || c == '\\' || c == ':') {
        return Err(Rejection::UnsafeName);
    }
    let (path, directory) = match text.strip_suffix('/') {
        Some(path) => (path, true),
        None => (text, false),
    };
    if path.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..") {
        return Err(Rejection::UnsafeName);
    }
    Ok(directory)
}

// ---- plugin bundles ----

const PLUGIN_MAGIC: &[u8; 8] = b"FCPLUGB1";
/// Matches `figcraft_plugin::manifest::MAX_MANIFEST_BYTES`.
const MAX_PLUGIN_MANIFEST: usize = 256 * 1024;
/// Matches the default `figcraft_plugin::sandbox::Limits::module_bytes`.
const MAX_PLUGIN_MODULE: usize = 16 * 1024 * 1024;
/// Matches `figcraft_plugin::bundle::MAX_ASSETS` and its asset-name length.
const MAX_PLUGIN_ASSETS: u32 = 32;
const MAX_ASSET_NAME: usize = 64;
const WASM_HEADER: &[u8; 8] = b"\0asm\x01\0\0\0";

/// Length-prefixed parts: a length that exceeds what is left is `Truncated`, never an allocation.
struct Parts<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Parts<'a> {
    fn u32(&mut self) -> Result<u32, Rejection> {
        let value = le32(self.bytes, self.at)?;
        self.at = add(self.at, 4)?;
        Ok(value)
    }

    fn part(&mut self, max: usize) -> Result<&'a [u8], Rejection> {
        let len = to_usize(self.u32()?)?;
        if len > max {
            return Err(Rejection::EntrySizeBudget);
        }
        let part = slice(self.bytes, self.at, len)?;
        self.at = add(self.at, len)?;
        Ok(part)
    }
}

fn plugin(bytes: &[u8], budgets: &Budgets) -> Result<Summary, Rejection> {
    let entry_max = usize::try_from(budgets.max_entry_bytes).unwrap_or(usize::MAX);
    let mut parts = Parts { bytes, at: PLUGIN_MAGIC.len() };
    let manifest = parts.part(MAX_PLUGIN_MANIFEST.min(entry_max))?;
    std::str::from_utf8(manifest).map_err(|_| Rejection::Malformed)?;
    let module = parts.part(MAX_PLUGIN_MODULE.min(entry_max))?;
    if !module.starts_with(WASM_HEADER) {
        return Err(Rejection::Malformed);
    }
    let assets = parts.u32()?;
    if assets > MAX_PLUGIN_ASSETS.min(budgets.max_entries) {
        return Err(Rejection::EntryCountBudget);
    }
    let mut names = BTreeSet::new();
    for _ in 0..assets {
        let name = parts.part(MAX_ASSET_NAME).map_err(|e| if e == Rejection::EntrySizeBudget { Rejection::UnsafeName } else { e })?;
        check_asset_name(name)?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(Rejection::DuplicateName);
        }
        parts.part(entry_max)?;
    }
    if parts.at != bytes.len() {
        return Err(Rejection::TrailingBytes);
    }
    Ok(Summary::Plugin { assets, module_bytes: u64::try_from(module.len()).unwrap_or(u64::MAX) })
}

/// The same rule as `figcraft_plugin::bundle::check_asset_name`: 1–64 ASCII letters, digits,
/// `.`, `-` or `_`, no leading or trailing dot, and no Windows device name.
fn check_asset_name(name: &[u8]) -> Result<(), Rejection> {
    let plain = !name.is_empty()
        && name.len() <= MAX_ASSET_NAME
        && name.first() != Some(&b'.')
        && name.last() != Some(&b'.')
        && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    let stem = name.split(|b| *b == b'.').next().unwrap_or_default().to_ascii_lowercase();
    let device = matches!(stem.as_slice(), b"con" | b"prn" | b"aux" | b"nul")
        || ((stem.starts_with(b"com") || stem.starts_with(b"lpt")) && stem.len() == 4 && stem.get(3).is_some_and(u8::is_ascii_digit));
    if !plain || device {
        return Err(Rejection::UnsafeName);
    }
    Ok(())
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
