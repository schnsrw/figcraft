//! Runtime branding for the hosted editor: the web container serves `/branding.json`, rendered
//! from its `FIGCRAFT_BRAND_*` environment (deploy/saas/branding.json.tmpl). The document is
//! untrusted: [`parse`] validates each field the editor uses and falls back to the default for
//! anything invalid. See docs/saas/branding-and-configuration.md.

/// The product name shown when no (valid) branding is served.
pub const DEFAULT_PRODUCT_NAME: &str = "FigCraft";
/// Longest product name kept, in characters (the shell uses the same cap).
pub const MAX_PRODUCT_NAME: usize = 64;
/// `/branding.json` is a few hundred bytes; a larger body is ignored.
pub const MAX_BRANDING_BYTES: usize = 16 * 1024;

/// The validated branding the editor applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branding {
    pub product_name: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self { product_name: DEFAULT_PRODUCT_NAME.to_string() }
    }
}

impl Branding {
    /// The document title.
    pub fn title(&self) -> String {
        self.product_name.clone()
    }

    /// The text shown while the editor starts.
    pub fn loading_text(&self) -> String {
        format!("Loading {}\u{2026}", self.product_name)
    }

    /// The text shown when the editor cannot start.
    pub fn failure_text(&self, error: &str) -> String {
        format!("{} failed to start: {error}. A browser with WebGPU or WebGL2 is required.", self.product_name)
    }
}

/// Parses an untrusted `/branding.json` body; never fails.
pub fn parse(body: &str) -> Branding {
    if body.len() > MAX_BRANDING_BYTES {
        return Branding::default();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Branding::default();
    };
    let product_name = value.get("productName").and_then(serde_json::Value::as_str).and_then(|s| clean_text(s, MAX_PRODUCT_NAME));
    Branding { product_name: product_name.unwrap_or_else(|| DEFAULT_PRODUCT_NAME.to_string()) }
}

/// Replaces control and invisible formatting characters with spaces, folds whitespace runs,
/// trims and caps the length in characters; `None` when nothing is left.
pub fn clean_text(s: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let mut pending_space = false;
    let mut count = 0usize;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() || is_invisible_format(c) {
            pending_space = !out.is_empty();
            continue;
        }
        if count >= max {
            break;
        }
        if pending_space {
            if count + 1 >= max {
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        out.push(c);
        count += 1;
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Unicode format characters (category Cf) that can reorder or hide text: bidi controls,
/// zero-width characters, the BOM, soft hyphen and friends.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_output_is_the_default() {
        let body = r##"{"version":1,"productName":"FigCraft","shortName":"FigCraft","primaryColor":"#b7df79","accentColor":"#94b85c","logoUrl":"","faviconUrl":"","supportUrl":"","termsUrl":"","privacyUrl":"","footerText":""}"##;
        assert_eq!(parse(body), Branding::default());
        assert_eq!(Branding::default().title(), "FigCraft");
        assert_eq!(Branding::default().loading_text(), "Loading FigCraft\u{2026}");
    }

    #[test]
    fn keeps_the_literal_escaped_name() {
        // What Caddy's `toJson` renders for `Acme "Design" \ </script>` plus a newline and emoji.
        let body = r#"{"productName":"Acme \"Design\" \\ </script>\nline2 ünï 🎨"}"#;
        assert_eq!(parse(body).product_name, "Acme \"Design\" \\ </script> line2 ünï 🎨");
    }

    #[test]
    fn falls_back_on_invalid_documents_and_fields() {
        for body in ["", "{", "null", "[]", "42", "\"FigCraft\"", "{}", r#"{"productName":7}"#, r#"{"productName":"  \u0000\u200b\n"}"#] {
            assert_eq!(parse(body), Branding::default(), "{body}");
        }
        let huge = format!(r#"{{"productName":"{}"}}"#, "x".repeat(MAX_BRANDING_BYTES));
        assert_eq!(parse(&huge), Branding::default());
    }

    #[test]
    fn caps_and_cleans_text() {
        assert_eq!(clean_text(&"a".repeat(65), 64).as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(clean_text(&"🎨".repeat(70), 64).map(|s| s.chars().count()), Some(64));
        assert_eq!(clean_text("  A\u{202E}B\u{200D}C  ", 64).as_deref(), Some("A B C"));
        assert_eq!(clean_text("a b", 2).as_deref(), Some("a"));
        assert_eq!(clean_text("\t\r\n", 64), None);
    }

    #[test]
    fn junk_sweep_never_panics_and_respects_the_caps() {
        let values =
            ["", " ", "\\", "\"", "</script>", "\u{0}", "\u{7f}", "\u{85}", "\u{2028}", "\u{FEFF}", "\u{10FFFF}", "e\u{301}", "🎨", "#gggggg"];
        let mut bodies: Vec<String> = Vec::new();
        for v in values {
            if let Ok(json) = serde_json::to_string(v) {
                bodies.push(format!(r#"{{"productName":{json}}}"#));
                bodies.push(format!(r#"{{"productName":{json},"productName":{json}}}"#));
            }
            bodies.push(v.to_string());
            bodies.push(format!(r#"{{"productName":"{v}"}}"#));
        }
        for n in [0, 1, 63, 64, 65, 1000] {
            bodies.push(format!(r#"{{"productName":"{}"}}"#, "é".repeat(n)));
        }
        for body in &bodies {
            let b = parse(body);
            let count = b.product_name.chars().count();
            assert!(count > 0 && count <= MAX_PRODUCT_NAME, "{body:?} -> {:?}", b.product_name);
            assert!(!b.product_name.chars().any(|c| c.is_control() || is_invisible_format(c)), "{body:?}");
            assert_eq!(b.product_name.trim(), b.product_name);
            let _ = (b.title(), b.loading_text(), b.failure_text("x"));
        }
        // Every truncation point of a valid document parses without panicking.
        let full = r#"{"productName":"Acme ünï 🎨"}"#;
        for (i, _) in full.char_indices() {
            let _ = parse(full.get(..i).unwrap_or_default());
        }
    }
}
