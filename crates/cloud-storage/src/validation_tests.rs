//! Upload validation: accepted samples, hostile samples and a never-crash junk sweep. Every sample
//! is synthetic and generated here.

use std::io::{Cursor, Write};

use super::{Budgets, MEDIA_FIGCRAFT, MEDIA_GIF, MEDIA_JPEG, MEDIA_PLUGIN, MEDIA_PNG, MEDIA_WEBP, MediaType, Rejection, Summary, validate};

const B: Budgets = Budgets::DEFAULT;

// ---- sample builders ----

fn png(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = b"IHDR".to_vec();
    chunk.extend(width.to_be_bytes());
    chunk.extend(height.to_be_bytes());
    chunk.extend([depth, color, 0, 0, 0]);
    out.extend(13u32.to_be_bytes());
    out.extend(&chunk);
    out.extend(crc32fast::hash(&chunk).to_be_bytes());
    // An empty IDAT and IEND so the sample is a complete file.
    for name in [b"IDAT", b"IEND"] {
        out.extend(0u32.to_be_bytes());
        out.extend(name);
        out.extend(crc32fast::hash(name).to_be_bytes());
    }
    out
}

fn jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    // APP0 with a JFIF-style payload, a fill byte, a standalone RST marker and a DQT.
    out.extend([0xFF, 0xE0, 0x00, 0x10]);
    out.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    out.extend([0xFF, 0xFF, 0xD0]);
    out.extend([0xFF, 0xDB, 0x00, 0x43, 0x00]);
    out.extend([1u8; 64]);
    out.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
    out.extend(height.to_be_bytes());
    out.extend(width.to_be_bytes());
    out.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    out.extend([0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0, 0x12, 0x34, 0xFF, 0xD9]);
    out
}

fn riff(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = fourcc.to_vec();
    chunk.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
    chunk.extend(data);
    if data.len() % 2 == 1 {
        chunk.push(0);
    }
    let mut out = b"RIFF".to_vec();
    out.extend(u32::try_from(chunk.len() + 4).unwrap().to_le_bytes());
    out.extend(b"WEBP");
    out.extend(chunk);
    out
}

fn webp_vp8(width: u16, height: u16) -> Vec<u8> {
    let mut data = vec![0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A];
    data.extend(width.to_le_bytes());
    data.extend(height.to_le_bytes());
    data.extend([0u8; 8]);
    riff(b"VP8 ", &data)
}

fn webp_vp8l(width: u32, height: u32) -> Vec<u8> {
    let bits = (width - 1) | ((height - 1) << 14);
    let mut data = vec![0x2F];
    data.extend(bits.to_le_bytes());
    data.extend([0u8; 4]);
    riff(b"VP8L", &data)
}

fn webp_vp8x(width: u32, height: u32) -> Vec<u8> {
    let mut data = vec![0u8; 4];
    data.extend(&(width - 1).to_le_bytes()[..3]);
    data.extend(&(height - 1).to_le_bytes()[..3]);
    riff(b"VP8X", &data)
}

fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend(width.to_le_bytes());
    out.extend(height.to_le_bytes());
    out.extend([0, 0, 0, 0x3B]);
    out
}

/// A `.figcraft`-shaped archive written by the `zip` crate, as `figcraft-format` writes one.
fn figcraft() -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("document.json", deflated).unwrap();
    z.write_all(br#"{"format":"figcraft","version":1,"pages":[{"name":"Page 1","children":[]}]}"#).unwrap();
    z.start_file("images/0a1b2c", stored).unwrap();
    z.write_all(&png(4, 4, 8, 6)).unwrap();
    z.start_file("library/manifest.json", deflated).unwrap();
    z.write_all(br#"{"id":"lib","name":"Library"}"#).unwrap();
    z.finish().unwrap();
    buf.into_inner()
}

/// One hand-built zip entry. `compressed` and `uncompressed` are what the headers declare.
#[derive(Clone)]
struct Entry {
    name: Vec<u8>,
    method: u16,
    flags: u16,
    data: Vec<u8>,
    uncompressed: u32,
}

impl Entry {
    fn stored(name: &str, data: &[u8]) -> Self {
        Self { name: name.as_bytes().to_vec(), method: 0, flags: 0, data: data.to_vec(), uncompressed: u32::try_from(data.len()).unwrap() }
    }

    fn deflated(name: &str, compressed: usize, uncompressed: u32) -> Self {
        Self { name: name.as_bytes().to_vec(), method: 8, flags: 0, data: vec![0u8; compressed], uncompressed }
    }
}

fn local_header(entry: &Entry) -> Vec<u8> {
    let mut out = 0x0403_4B50u32.to_le_bytes().to_vec();
    out.extend(20u16.to_le_bytes());
    out.extend(entry.flags.to_le_bytes());
    out.extend(entry.method.to_le_bytes());
    out.extend([0u8; 8]); // time, date, crc
    out.extend(u32::try_from(entry.data.len()).unwrap().to_le_bytes());
    out.extend(entry.uncompressed.to_le_bytes());
    out.extend(u16::try_from(entry.name.len()).unwrap().to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(&entry.name);
    out
}

fn central_header(entry: &Entry, offset: u32) -> Vec<u8> {
    let mut out = 0x0201_4B50u32.to_le_bytes().to_vec();
    out.extend(20u16.to_le_bytes());
    out.extend(20u16.to_le_bytes());
    out.extend(entry.flags.to_le_bytes());
    out.extend(entry.method.to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(u32::try_from(entry.data.len()).unwrap().to_le_bytes());
    out.extend(entry.uncompressed.to_le_bytes());
    out.extend(u16::try_from(entry.name.len()).unwrap().to_le_bytes());
    out.extend([0u8; 12]); // extra, comment, disk, attributes
    out.extend(offset.to_le_bytes());
    out.extend(&entry.name);
    out
}

fn eocd(entries: u16, size: u32, offset: u32) -> Vec<u8> {
    let mut out = 0x0605_4B50u32.to_le_bytes().to_vec();
    out.extend([0u8; 4]);
    out.extend(entries.to_le_bytes());
    out.extend(entries.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend(offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn raw_zip(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let offset = u32::try_from(out.len()).unwrap();
        out.extend(local_header(entry));
        out.extend(&entry.data);
        directory.extend(central_header(entry, offset));
    }
    let start = u32::try_from(out.len()).unwrap();
    let size = u32::try_from(directory.len()).unwrap();
    out.extend(directory);
    out.extend(eocd(u16::try_from(entries.len()).unwrap(), size, start));
    out
}

fn with_document(mut entries: Vec<Entry>) -> Vec<u8> {
    entries.insert(0, Entry::stored("document.json", b"{}"));
    raw_zip(&entries)
}

fn plugin(manifest: &[u8], module: &[u8], assets: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = b"FCPLUGB1".to_vec();
    for part in [manifest, module] {
        out.extend(u32::try_from(part.len()).unwrap().to_le_bytes());
        out.extend(part);
    }
    out.extend(u32::try_from(assets.len()).unwrap().to_le_bytes());
    for (name, data) in assets {
        for part in [*name, *data] {
            out.extend(u32::try_from(part.len()).unwrap().to_le_bytes());
            out.extend(part);
        }
    }
    out
}

const WASM: &[u8] = b"\0asm\x01\0\0\0";
const MANIFEST: &[u8] = br#"{"id":"org.example.p","name":"P","version":"1.0.0","main":"p.wasm","api":1}"#;

fn valid_plugin() -> Vec<u8> {
    plugin(MANIFEST, WASM, &[(b"icon.png", &[1, 2, 3]), (b"LICENSE.txt", b"MIT")])
}

fn samples() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (MEDIA_PNG, png(640, 480, 8, 6)),
        (MEDIA_JPEG, jpeg(800, 600)),
        (MEDIA_WEBP, webp_vp8(320, 200)),
        (MEDIA_WEBP, webp_vp8l(321, 201)),
        (MEDIA_WEBP, webp_vp8x(1920, 1080)),
        (MEDIA_GIF, gif(16, 9)),
        (MEDIA_FIGCRAFT, figcraft()),
        (MEDIA_FIGCRAFT, with_document(vec![Entry::stored("images/", b""), Entry::deflated("versions/1/document.json", 100, 2000)])),
        (MEDIA_PLUGIN, valid_plugin()),
    ]
}

fn reject(media: &str, bytes: &[u8]) -> Rejection {
    validate(media, bytes, &B).unwrap_err()
}

fn image(media: &str, bytes: &[u8]) -> (u32, u32, u64) {
    match validate(media, bytes, &B).unwrap().summary {
        Summary::Image { width, height, decoded_bytes } => (width, height, decoded_bytes),
        other => panic!("not an image: {other:?}"),
    }
}

// ---- accepted samples ----

#[test]
fn valid_images_report_header_dimensions() {
    assert_eq!(image(MEDIA_PNG, &png(640, 480, 8, 6)), (640, 480, 640 * 480 * 4));
    assert_eq!(image(MEDIA_PNG, &png(10, 10, 16, 2)), (10, 10, 800), "16-bit samples need 8 bytes a pixel");
    assert_eq!(image(MEDIA_PNG, &png(3, 2, 1, 3)), (3, 2, 24));
    assert_eq!(image(MEDIA_JPEG, &jpeg(800, 600)), (800, 600, 800 * 600 * 4));
    assert_eq!(image(MEDIA_WEBP, &webp_vp8(320, 200)), (320, 200, 320 * 200 * 4));
    assert_eq!(image(MEDIA_WEBP, &webp_vp8l(321, 201)), (321, 201, 321 * 201 * 4));
    assert_eq!(image(MEDIA_WEBP, &webp_vp8x(1920, 1080)), (1920, 1080, 1920 * 1080 * 4));
    assert_eq!(image(MEDIA_GIF, &gif(16, 9)), (16, 9, 16 * 9 * 4));
    assert_eq!(image("IMAGE/PNG", &png(1, 1, 8, 0)), (1, 1, 4), "media types compare without ASCII case");
}

#[test]
fn archives_and_plugins_report_their_contents() {
    let figcraft = validate(MEDIA_FIGCRAFT, &figcraft(), &B).unwrap();
    assert_eq!(figcraft.media, MediaType::Figcraft);
    let Summary::Archive { entries, expanded_bytes } = figcraft.summary else { panic!("not an archive") };
    assert_eq!(entries, 3);
    assert!(expanded_bytes > 100);
    let raw = validate(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("a", 10, 2000)]), &B).unwrap();
    assert_eq!(raw.summary, Summary::Archive { entries: 2, expanded_bytes: 2002 });
    let bundle = validate(MEDIA_PLUGIN, &valid_plugin(), &B).unwrap();
    assert_eq!(bundle.summary, Summary::Plugin { assets: 2, module_bytes: 8 });
}

// ---- media type and sniffing ----

#[test]
fn declared_type_must_be_supported_and_match_the_magic_bytes() {
    assert_eq!(reject(MEDIA_JPEG, &png(1, 1, 8, 6)), Rejection::MediaMismatch);
    assert_eq!(reject(MEDIA_PNG, &gif(1, 1)), Rejection::MediaMismatch);
    assert_eq!(reject(MEDIA_PLUGIN, &figcraft()), Rejection::MediaMismatch);
    assert_eq!(reject(MEDIA_PNG, b"hello"), Rejection::MediaMismatch);
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
    for media in ["image/svg+xml", "application/pdf", "text/html", "application/zip", "image/png; charset=binary", " image/png", ""] {
        assert_eq!(reject(media, svg), Rejection::UnsupportedMediaType, "{media}");
    }
    assert_eq!(reject("image/svg+xml", b"%PDF-1.7"), Rejection::UnsupportedMediaType);
    assert_eq!(reject(MEDIA_PNG, b""), Rejection::Empty);
    let tight = Budgets { max_input_bytes: 10, ..B };
    assert_eq!(validate(MEDIA_PNG, &png(1, 1, 8, 6), &tight).unwrap_err(), Rejection::InputTooLarge);
    for media in MediaType::ALL {
        assert_eq!(MediaType::from_declared(media.as_str()), Some(media));
    }
}

// ---- image budgets ----

#[test]
fn huge_and_zero_dimensions_are_refused_from_the_header() {
    assert_eq!(reject(MEDIA_PNG, &png(20_000, 20_000, 8, 6)), Rejection::PixelBudget);
    assert_eq!(reject(MEDIA_PNG, &png(0x7FFF_FFFF, 1, 8, 6)), Rejection::DimensionTooLarge);
    assert_eq!(reject(MEDIA_PNG, &png(0x8000_0000, 1, 8, 6)), Rejection::Malformed);
    assert_eq!(reject(MEDIA_PNG, &png(0, 10, 8, 6)), Rejection::ZeroDimension);
    assert_eq!(reject(MEDIA_PNG, &png(10_000, 9_000, 16, 6)), Rejection::DecodedByteBudget, "90 MP at 16 bits is 720 MB");
    let tight = Budgets { max_decoded_bytes: 100, ..B };
    assert_eq!(validate(MEDIA_PNG, &png(10, 10, 8, 6), &tight).unwrap_err(), Rejection::DecodedByteBudget);
    assert_eq!(reject(MEDIA_JPEG, &jpeg(65_535, 65_535)), Rejection::PixelBudget);
    assert_eq!(reject(MEDIA_JPEG, &jpeg(100, 0)), Rejection::ZeroDimension);
    assert_eq!(reject(MEDIA_WEBP, &webp_vp8x(1 << 24, 1)), Rejection::DimensionTooLarge);
    assert_eq!(reject(MEDIA_WEBP, &webp_vp8x(16_384, 16_384)), Rejection::PixelBudget);
    assert_eq!(reject(MEDIA_WEBP, &webp_vp8(0, 5)), Rejection::ZeroDimension);
    assert_eq!(reject(MEDIA_WEBP, &webp_vp8l(16_384, 16_384)), Rejection::PixelBudget);
    assert_eq!(reject(MEDIA_GIF, &gif(0, 1)), Rejection::ZeroDimension);
    let small = Budgets { max_dimension: 1000, ..B };
    assert_eq!(validate(MEDIA_GIF, &gif(1001, 1), &small).unwrap_err(), Rejection::DimensionTooLarge);
}

#[test]
fn image_headers_are_checked_strictly() {
    let mut bad_crc = png(8, 8, 8, 6);
    bad_crc[20] ^= 1;
    assert_eq!(reject(MEDIA_PNG, &bad_crc), Rejection::HeaderChecksum);
    assert_eq!(reject(MEDIA_PNG, &png(8, 8, 3, 6)), Rejection::Malformed, "RGBA has no 3-bit depth");
    assert_eq!(reject(MEDIA_PNG, &png(8, 8, 8, 5)), Rejection::Malformed, "no colour type 5");
    assert_eq!(reject(MEDIA_PNG, &png(8, 8, 8, 6)[..20]), Rejection::Truncated);

    let mut scan_first = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
    assert_eq!(reject(MEDIA_JPEG, &scan_first), Rejection::Malformed, "a scan before any frame header");
    scan_first[3] = 0xC4;
    assert_eq!(reject(MEDIA_JPEG, &scan_first), Rejection::Truncated, "a DHT is not a frame header");
    let mut endless = vec![0xFF, 0xD8];
    for _ in 0..2000 {
        endless.extend([0xFF, 0xFE, 0x00, 0x02]);
    }
    assert_eq!(reject(MEDIA_JPEG, &endless), Rejection::Malformed, "segment walk is bounded");
    let mut bad_length = jpeg(4, 4);
    bad_length[5] = 1;
    assert_eq!(reject(MEDIA_JPEG, &bad_length), Rejection::Malformed);

    let mut extra = webp_vp8(4, 4);
    extra.extend([0, 0]);
    assert_eq!(reject(MEDIA_WEBP, &extra), Rejection::TrailingBytes);
    let mut inter_frame = webp_vp8(4, 4);
    inter_frame[20] |= 1;
    assert_eq!(reject(MEDIA_WEBP, &inter_frame), Rejection::Malformed);
    assert_eq!(reject(MEDIA_WEBP, &riff(b"ALPH", &[0; 10])), Rejection::Malformed);
    let mut short_chunk = webp_vp8x(4, 4);
    short_chunk[16] = 200;
    assert_eq!(reject(MEDIA_WEBP, &short_chunk), Rejection::Truncated);
}

// ---- archive budgets ----

#[test]
fn zip_bombs_by_declared_ratio_and_size_are_refused() {
    // 10 KiB declaring 100 MiB: a 10 240:1 ratio.
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("bomb", 10 * 1024, 100 << 20)])), Rejection::CompressionRatio);
    // Just within the ratio passes; below the floor any ratio passes.
    assert!(validate(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("ok", 9 * 1024, 2 << 20)]), &B).is_ok());
    assert!(validate(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("tiny", 1, 1 << 20)]), &B).is_ok());
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("empty", 0, (1 << 20) + 1)])), Rejection::CompressionRatio);
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::deflated("big", 4 << 20, 600 << 20)])), Rejection::EntrySizeBudget);

    // Many entries each within budget, together over the expanded-byte budget.
    let tight = Budgets { max_expanded_bytes: 5000, ..B };
    let many: Vec<Entry> = (0..4).map(|i| Entry::deflated(&format!("part{i}"), 100, 2000)).collect();
    assert_eq!(validate(MEDIA_FIGCRAFT, &with_document(many), &tight).unwrap_err(), Rejection::ExpandedByteBudget);

    let few = Budgets { max_entries: 3, ..B };
    let four: Vec<Entry> = (0..3).map(|i| Entry::stored(&format!("f{i}"), b"x")).collect();
    assert_eq!(validate(MEDIA_FIGCRAFT, &with_document(four), &few).unwrap_err(), Rejection::EntryCountBudget);
}

#[test]
fn traversal_absolute_control_and_duplicate_names_are_refused() {
    for name in ["../evil", "/etc/passwd", "a/../b", "a/./b", "./a", "a//b", "C:\\x", "C:/x", "a\\b", "a\u{0}b", "a\nb", "dir//", ".."] {
        assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored(name, b"x")])), Rejection::UnsafeName, "{name:?}");
    }
    let mut bad_utf8 = Entry::stored("x", b"x");
    bad_utf8.name = vec![b'a', 0xFF];
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![bad_utf8])), Rejection::UnsafeName);
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored(&"n".repeat(256), b"x")])), Rejection::UnsafeName);
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored("dir/", b"x")])), Rejection::Malformed, "directories hold no bytes");
    assert_eq!(
        reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored("images/a", b"1"), Entry::stored("Images/A", b"2")])),
        Rejection::DuplicateName
    );
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored("document.json", b"2")])), Rejection::DuplicateName);
    assert!(validate(MEDIA_FIGCRAFT, &with_document(vec![Entry::stored("images/é.png", b"x")]), &B).is_ok());
}

#[test]
fn unsupported_zip_features_are_refused() {
    let mut encrypted = Entry::stored("secret", b"x");
    encrypted.flags = 1;
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![encrypted])), Rejection::Encrypted);
    let mut aes = Entry::stored("aes", b"x");
    aes.method = 99;
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![aes])), Rejection::Encrypted);
    let mut bzip = Entry::stored("bz", b"x");
    bzip.method = 12;
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![bzip])), Rejection::UnsupportedCompression);
    let mut lying = Entry::stored("stored", b"abc");
    lying.uncompressed = 3000;
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![lying])), Rejection::Malformed, "stored sizes must agree");
    let mut zip64 = Entry::stored("z64", b"x");
    zip64.uncompressed = u32::MAX;
    zip64.method = 8;
    assert_eq!(reject(MEDIA_FIGCRAFT, &with_document(vec![zip64])), Rejection::Zip64Unsupported);

    // A zip64 end-of-directory locator in front of the record.
    let mut archive = with_document(vec![]);
    let eocd_at = archive.len() - 22;
    let mut locator = 0x0706_4B50u32.to_le_bytes().to_vec();
    locator.extend([0u8; 16]);
    archive.splice(eocd_at..eocd_at, locator);
    assert_eq!(reject(MEDIA_FIGCRAFT, &archive), Rejection::Zip64Unsupported);

    assert_eq!(reject(MEDIA_FIGCRAFT, &raw_zip(&[Entry::stored("other.json", b"{}")])), Rejection::MissingMember);
}

#[test]
fn layout_tricks_are_refused() {
    let good = with_document(vec![Entry::stored("a", b"hello")]);
    // Bytes appended after the record (the comment length no longer ends the file).
    let mut trailing = good.clone();
    trailing.extend(b"junk");
    assert_eq!(reject(MEDIA_FIGCRAFT, &trailing), Rejection::Malformed);
    // Bytes between the directory and the record.
    let mut gap = good.clone();
    let at = gap.len() - 22;
    gap.splice(at..at, [0u8; 4]);
    assert_eq!(reject(MEDIA_FIGCRAFT, &gap), Rejection::Malformed);
    // A valid comment is fine.
    let mut commented = good.clone();
    let len = commented.len();
    commented[len - 2..].copy_from_slice(&5u16.to_le_bytes());
    commented.extend(b"hello");
    assert!(validate(MEDIA_FIGCRAFT, &commented, &B).is_ok());

    // An overlap bomb: entry `b` lives inside entry `a`'s stored data.
    let b = Entry::stored("b", b"inner");
    let mut a_data = local_header(&b);
    a_data.extend(&b.data);
    let doc = Entry::stored("document.json", b"{}");
    let a = Entry::stored("a", &a_data);
    let mut out = local_header(&doc);
    out.extend(&doc.data);
    let a_offset = u32::try_from(out.len()).unwrap();
    out.extend(local_header(&a));
    out.extend(&a.data);
    let b_offset = a_offset + u32::try_from(local_header(&a).len()).unwrap();
    let start = u32::try_from(out.len()).unwrap();
    let mut directory = central_header(&doc, 0);
    directory.extend(central_header(&a, a_offset));
    directory.extend(central_header(&b, b_offset));
    let size = u32::try_from(directory.len()).unwrap();
    out.extend(directory);
    out.extend(eocd(3, size, start));
    assert_eq!(reject(MEDIA_FIGCRAFT, &out), Rejection::OverlappingEntries);

    // A central entry whose local header names something else.
    let mut renamed = good.clone();
    let local_name = renamed.iter().position(|b| *b == b'd').unwrap();
    renamed[local_name] = b'D';
    assert_eq!(reject(MEDIA_FIGCRAFT, &renamed), Rejection::Malformed);
}

// ---- plugin bundles ----

#[test]
fn plugin_bundles_are_bounded_and_named_safely() {
    assert_eq!(reject(MEDIA_PLUGIN, &plugin(MANIFEST, b"not wasm", &[])), Rejection::Malformed);
    assert_eq!(reject(MEDIA_PLUGIN, &plugin(&[0xFF, 0xFE], WASM, &[])), Rejection::Malformed);
    let big_manifest = vec![b' '; 256 * 1024 + 1];
    assert_eq!(reject(MEDIA_PLUGIN, &plugin(&big_manifest, WASM, &[])), Rejection::EntrySizeBudget);
    let assets: Vec<(Vec<u8>, Vec<u8>)> = (0..33).map(|i| (format!("a{i}").into_bytes(), vec![])).collect();
    let refs: Vec<(&[u8], &[u8])> = assets.iter().map(|(n, d)| (n.as_slice(), d.as_slice())).collect();
    assert_eq!(reject(MEDIA_PLUGIN, &plugin(MANIFEST, WASM, &refs)), Rejection::EntryCountBudget);
    for name in [&b"../x"[..], b"a/b", b".hidden", b"trail.", b"con", b"COM1.txt", b"", &[b'n'; 65]] {
        assert_eq!(reject(MEDIA_PLUGIN, &plugin(MANIFEST, WASM, &[(name, b"x")])), Rejection::UnsafeName, "{name:?}");
    }
    assert_eq!(reject(MEDIA_PLUGIN, &plugin(MANIFEST, WASM, &[(b"a.png", b"1"), (b"A.PNG", b"2")])), Rejection::DuplicateName);
    let mut trailing = valid_plugin();
    trailing.push(0);
    assert_eq!(reject(MEDIA_PLUGIN, &trailing), Rejection::TrailingBytes);
    let mut long_length = valid_plugin();
    long_length[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(reject(MEDIA_PLUGIN, &long_length), Rejection::EntrySizeBudget | Rejection::Malformed));
    let tight = Budgets { max_entry_bytes: 2, ..B };
    assert_eq!(validate(MEDIA_PLUGIN, &plugin(MANIFEST, WASM, &[]), &tight).unwrap_err(), Rejection::EntrySizeBudget);
}

#[test]
fn rejection_codes_are_fixed_snake_case() {
    for code in [Rejection::Empty, Rejection::CompressionRatio, Rejection::Zip64Unsupported, Rejection::MissingMember] {
        assert!(code.as_str().bytes().all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit()));
        assert_eq!(code.to_string(), code.as_str());
    }
}

// ---- never-crash sweep ----

/// A small deterministic xorshift generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

fn every_media() -> Vec<&'static str> {
    MediaType::ALL.iter().map(|m| m.as_str()).collect()
}

#[test]
fn every_truncation_of_every_sample_is_handled() {
    for (media, bytes) in samples() {
        assert!(validate(media, &bytes, &B).is_ok(), "{media} sample is valid");
        for len in 0..bytes.len() {
            let result = validate(media, &bytes[..len], &B);
            // Containers must span the upload exactly, so any cut is refused.
            if media == MEDIA_FIGCRAFT || media == MEDIA_PLUGIN || media == MEDIA_WEBP {
                assert!(result.is_err(), "{media} cut to {len} bytes was accepted");
            }
        }
    }
}

#[test]
fn every_byte_flip_of_every_sample_is_handled() {
    for (media, bytes) in samples() {
        for at in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xFF] {
                let mut flipped = bytes.clone();
                flipped[at] ^= mask;
                for declared in every_media() {
                    let _ = validate(declared, &flipped, &B);
                }
                let _ = validate(media, &flipped, &Budgets { max_entries: 1, max_ratio: 1, ratio_floor_bytes: 0, ..B });
            }
        }
    }
}

#[test]
fn random_garbage_and_mutations_never_crash() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let magics: [&[u8]; 8] =
        [b"\x89PNG\r\n\x1a\n", &[0xFF, 0xD8, 0xFF], b"GIF89a", b"RIFF\x10\0\0\0WEBP", b"PK\x03\x04", b"PK\x05\x06", b"FCPLUGB1", b""];
    let samples = samples();
    for round in 0..30_000 {
        let mut bytes = if round % 3 == 0 {
            let (_, sample) = &samples[rng.below(samples.len())];
            let mut mutated = sample.clone();
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(mutated.len());
                mutated[at] = u8::try_from(rng.next() & 0xFF).unwrap();
            }
            if rng.below(4) == 0 {
                mutated.truncate(rng.below(mutated.len() + 1));
            }
            mutated
        } else {
            let mut garbage = magics[rng.below(magics.len())].to_vec();
            let len = rng.below(600);
            garbage.extend((0..len).map(|_| u8::try_from(rng.next() & 0xFF).unwrap()));
            garbage
        };
        // Sometimes end with a plausible end-of-directory record pointing anywhere.
        if round % 5 == 0 {
            let entries = u16::try_from(rng.below(4)).unwrap();
            let size = u32::try_from(rng.below(bytes.len() + 64)).unwrap();
            let offset = u32::try_from(rng.below(bytes.len() + 64)).unwrap();
            bytes.extend(eocd(entries, size, offset));
        }
        for declared in every_media() {
            let _ = validate(declared, &bytes, &B);
        }
    }
}
