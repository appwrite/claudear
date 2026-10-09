//! Escaping for notifier channels that render HTML.

/// Escape `text` so an HTML renderer shows it verbatim, inside element text or a
/// double-quoted attribute value.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escapes_markup_characters() {
        assert_eq!(
            escape("<a href=\"x\">Tom & Jerry</a>"),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&lt;/a&gt;"
        );
    }

    #[test]
    fn escapes_existing_entities_again() {
        assert_eq!(escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn leaves_plain_text_unchanged() {
        assert_eq!(escape("PROJ-1 'quoted' ünïcode"), "PROJ-1 'quoted' ünïcode");
    }
}
