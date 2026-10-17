//! The page's `#join=<invite link>` fragment: opening `/edit/#join=<url-encoded link>` joins that
//! live session at once (a cloud share link, or any invite the Join dialog takes).
//!
//! The link is read from the fragment (never the query: a fragment isn't sent to the server, so
//! the secret stays out of access logs) and the fragment is then cleared from the address bar.

/// Longest fragment read (an invite link is at most 2048 bytes; encoded it can triple).
const MAX_HASH: usize = 8192;
/// Longest decoded link handed on (the invite parser refuses longer ones anyway).
const MAX_LINK: usize = 2048;

/// The invite link in a `location.hash` like `#join=ws%3A%2F%2F…`, decoded; `None` when there's
/// no `join` entry, it's empty, too long, or not valid percent-encoded UTF-8. Other `&`-separated
/// entries are ignored; the first `join` wins. The link is not validated here (joining does that,
/// with errors that never echo it).
pub fn join_link(hash: &str) -> Option<String> {
    if hash.len() > MAX_HASH {
        return None;
    }
    let body = hash.strip_prefix('#').unwrap_or(hash);
    let value = body.split('&').find_map(|entry| entry.strip_prefix("join="))?;
    let link = percent_decode(value)?;
    let link = link.trim();
    if link.is_empty() || link.len() > MAX_LINK || link.chars().any(char::is_control) {
        return None;
    }
    Some(link.to_string())
}

/// `%XX` escapes and `+` (a space, as form encoding writes it) decoded; `None` on a broken escape
/// or bytes that aren't UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hi = hex(*bytes.get(i + 1)?)?;
                let lo = hex(*bytes.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::join_link;

    const LINK: &str = "ws://127.0.0.1:8080/ws/v1/collab?file=0b0e7f5c-6a51-4c8e-9a43-0d2f1e3c4b5a&token=AbCdEfGhIjKlMnOpQrStUv";

    fn encode(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }

    #[test]
    fn reads_an_encoded_invite() {
        assert_eq!(join_link(&format!("#join={}", encode(LINK))).as_deref(), Some(LINK));
        // Without the `#`, lower-case escapes, alongside other entries, unencoded.
        assert_eq!(join_link(&format!("join={}", encode(LINK).to_lowercase())).as_deref(), Some(LINK.to_lowercase().as_str()));
        assert_eq!(join_link(&format!("#x=1&join={}&y", encode(LINK))).as_deref(), Some(LINK));
        assert_eq!(join_link("#join=figcraft://join?host=127.0.0.1:47900").as_deref(), Some("figcraft://join?host=127.0.0.1:47900"));
        // The decoded link is a link the collab parser takes.
        let invite = figcraft_collab_net::invite::Invite::parse(&join_link(&format!("#join={}", encode(LINK))).unwrap_or_default());
        let invite = invite.unwrap();
        assert_eq!(invite.file, "0b0e7f5c-6a51-4c8e-9a43-0d2f1e3c4b5a");
        assert_eq!(invite.token, "AbCdEfGhIjKlMnOpQrStUv");
        assert_eq!(invite.server.path, "/ws/v1/collab/");
        assert!(!invite.server.secure);
    }

    #[test]
    fn ignores_anything_else() {
        for hash in [
            "",
            "#",
            "#join",
            "#join=",
            "#join=%20%20",
            "#other=ws%3A%2F%2Fx",
            "#join=%",
            "#join=%4",
            "#join=%zz",
            "#join=%FF%FE",
            "#join=a%0Ab",
            "#join=%00",
        ] {
            assert_eq!(join_link(hash), None, "{hash:?}");
        }
        assert_eq!(join_link(&format!("#join={}", "a".repeat(3000))), None, "too long decoded");
        assert_eq!(join_link(&format!("#join={}", "%41".repeat(3000))), None, "too long encoded");
    }

    /// Junk sweep: no input panics, and whatever comes back is bounded and printable.
    #[test]
    fn junk_never_panics() {
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let alphabet: &[u8] = b"#join=%&+?:/0123456789abcdefABCDEFxyz \t\n\x00\xC3\xA9";
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|k| {
                    let r = seed.rotate_left(k as u32 * 5);
                    alphabet.get((r % alphabet.len() as u64) as usize).copied().unwrap_or(b'x')
                })
                .collect();
            let s = String::from_utf8_lossy(&bytes);
            for hash in [s.to_string(), format!("#join={s}"), format!("#join=%{s}")] {
                if let Some(link) = join_link(&hash) {
                    assert!(!link.is_empty() && link.len() <= 2048 && !link.chars().any(char::is_control));
                }
            }
        }
        for s in ["%", "%%", "%%%", "#join=%%41", "#join=é%C3", "#join=%C3%A9", "\u{10FFFF}"] {
            let _ = join_link(s);
        }
    }
}
