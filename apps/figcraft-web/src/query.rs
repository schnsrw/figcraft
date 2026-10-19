//! The page's URL: the invite link its fragment names.

/// The invite link in `#join=<link>` (`fragment` is `location.hash`, with or without its `#`), still
/// escaped. The fragment, unlike the query, never leaves the browser, so the link's secret stays out
/// of the web host's logs and `Referer` headers. `join` takes the rest of the fragment, so a pasted
/// `figcraft://join?host=…&file=…&token=…` keeps its own `&`s. Anything longer than an escaped invite
/// link can be is ignored; what is left is checked by `collab.join` like a link the desktop app is
/// opened with.
/// The file name in `#name=<escaped>&join=<link>` (still escaped), which the web app puts before the
/// invite link so a file that has no name of its own yet opens under the name the workspace gave it.
/// Only the part before `&join=` is read, at most [`MAX_NAME`] bytes escaped.
pub fn join_name(fragment: &str) -> Option<&str> {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    let rest = fragment.strip_prefix("name=")?;
    let name = rest.get(..rest.find("&join=")?)?;
    (!name.is_empty() && name.len() <= MAX_NAME).then_some(name)
}

/// Longest escaped file name accepted (a name is at most 80 characters; every byte escaped is three).
const MAX_NAME: usize = 3 * 4 * 80;

pub fn join_link(fragment: &str) -> Option<&str> {
    // An invite link is at most 2 KiB; every byte of it escaped is three.
    const MAX: usize = 3 * 2048;
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    let start = if fragment.starts_with("join=") { 0 } else { fragment.find("&join=")? + 1 };
    fragment.get(start + "join=".len()..).filter(|link| !link.is_empty() && link.len() <= MAX)
}

#[cfg(test)]
mod tests {
    use super::{join_link, join_name};

    #[test]
    fn the_name_comes_before_the_link_and_the_link_keeps_its_own_ampersands() {
        let fragment = "#name=Home%20screen&join=ws%3A%2F%2Fh%2Fws%2Fv1%2Fcollab%3Ffile%3Dx%26token%3Dy";
        assert_eq!(join_name(fragment), Some("Home%20screen"));
        assert_eq!(join_link(fragment), Some("ws%3A%2F%2Fh%2Fws%2Fv1%2Fcollab%3Ffile%3Dx%26token%3Dy"));
        // Without a name the link still works and there is no name.
        assert_eq!(join_name("#join=abc"), None);
        assert_eq!(join_link("#join=abc"), Some("abc"));
    }

    #[test]
    fn a_missing_empty_or_oversize_name_is_ignored() {
        assert_eq!(join_name("#name=&join=abc"), None);
        assert_eq!(join_name("#name=x"), None);
        assert_eq!(join_name("#other=1&name=x&join=abc"), None);
        assert_eq!(join_name(&format!("#name={}&join=abc", "a".repeat(3 * 4 * 80 + 1))), None);
        assert_eq!(join_name(&format!("#name={}&join=abc", "a".repeat(3 * 4 * 80))).map(str::len), Some(3 * 4 * 80));
    }
}
