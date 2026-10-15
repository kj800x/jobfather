use pulldown_cmark::{Event, Options, Parser, html};

/// Render job-supplied markdown to HTML. Raw HTML in the source is shown as
/// escaped text rather than passed through.
pub fn render(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_markdown() {
        let html = render("# Title\n\n- **bold** item\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<table>"));
    }

    #[test]
    fn escapes_raw_html() {
        let html = render("hi <script>alert(1)</script>\n\n<div onclick=\"x\">block</div>\n");
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<div onclick"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
