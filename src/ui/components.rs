//! Shared HTML primitives used across every UI view.

/// Escape the five HTML-significant characters. Applied to every dynamic value
/// rendered into a page — statuses in particular carry arbitrary error text.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `1 branch` / `2 branches` — plain-English counts for meta lines.
// First consumed by the overview/project views (later tasks); defined here now.
#[allow(dead_code)]
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The brand mark: a single host fanning out to three branch endpoints.
pub const MARK: &str = r##"<svg class="mark" viewBox="0 0 32 32" fill="none" aria-hidden="true"><defs><linearGradient id="hg" x1="0" y1="0" x2="32" y2="32"><stop stop-color="#7b8cff"/><stop offset="1" stop-color="#a97bff"/></linearGradient></defs><circle cx="6" cy="16" r="3.2" fill="url(#hg)"/><circle cx="26" cy="7" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="16" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="25" r="2.6" fill="currentColor" opacity=".85"/><path d="M9 16H16M16 16V7H23M16 16H23M16 16V25H23" stroke="url(#hg)" stroke-width="1.6" stroke-linecap="round"/></svg>"##;

/// External-link glyph for URL chips.
pub const EXT_ICON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M7 17 17 7M9 7h8v8"/></svg>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(
            html_escape("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }
}
