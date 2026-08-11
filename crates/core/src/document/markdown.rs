//! Markdown, rendered by turning it into XHTML and handing it to the HTML engine.
//!
//! There is no separate `Document` implementation: `.md` *is* an `HtmlDocument`
//! whose text was synthesised instead of read. That keeps reflow, pagination,
//! links, the reader view and — importantly — the reading position, which is
//! keyed by the file's path, exactly as they are for `.html`.
//!
//! The one hard requirement on the generated markup is that it must parse as
//! XML, because that is what `html::xml::XmlParser` expects. `pulldown-cmark`'s
//! own output is well formed; raw HTML embedded in the Markdown is not, so it
//! is dropped rather than passed through.

use std::fs;
use std::path::Path;
use anyhow::Error;
use pulldown_cmark::{Parser, Options, Event, Tag, TagEnd, MetadataBlockKind, html};

use super::html::HtmlDocument;

pub const VIEWER_STYLESHEET: &str = "css/md.css";
pub const USER_STYLESHEET: &str = "css/md-user.css";

fn options() -> Options {
    Options::ENABLE_TABLES |
    Options::ENABLE_FOOTNOTES |
    Options::ENABLE_STRIKETHROUGH |
    Options::ENABLE_TASKLISTS |
    Options::ENABLE_SMART_PUNCTUATION |
    Options::ENABLE_HEADING_ATTRIBUTES |
    Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

fn escape(text: &str) -> String {
    let mut buf = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '>' => buf.push_str("&gt;"),
            '"' => buf.push_str("&quot;"),
            _ => buf.push(c),
        }
    }
    buf
}

/// The first ATX heading, used as the document title when there's no front matter.
fn first_heading(text: &str) -> Option<String> {
    let mut parser = Parser::new_ext(text, options());
    let mut buf = String::new();
    let mut inside = false;
    while let Some(event) = parser.next() {
        match event {
            Event::Start(Tag::Heading { .. }) => inside = true,
            Event::End(TagEnd::Heading(..)) if inside => break,
            Event::Text(t) | Event::Code(t) if inside => buf.push_str(&t),
            _ => (),
        }
    }
    Some(buf.trim().to_string()).filter(|s| !s.is_empty())
}

/// `title:` and `author:` out of a YAML front matter block, if there is one.
///
/// Deliberately not a YAML parser: front matter in the notes this has to read is
/// flat `key: value` lines, and a wrong guess here costs a library entry, not a
/// crash.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut parser = Parser::new_ext(text, options());
    let mut block = String::new();
    let mut inside = false;
    while let Some(event) = parser.next() {
        match event {
            Event::Start(Tag::MetadataBlock(MetadataBlockKind::YamlStyle)) => inside = true,
            Event::End(TagEnd::MetadataBlock(..)) => break,
            Event::Text(t) if inside => block.push_str(&t),
            _ => (),
        }
    }

    let mut title = None;
    let mut author = None;
    for line in block.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            if value.is_empty() {
                continue;
            }
            match key.trim().to_lowercase().as_ref() {
                "title" => title = Some(value),
                "author" | "authors" => author = Some(value),
                _ => (),
            }
        }
    }

    (title, author)
}

/// Markdown to a self-contained XHTML document.
pub fn to_html(text: &str, fallback_title: Option<&str>) -> String {
    let (meta_title, author) = front_matter(text);
    let title = meta_title.or_else(|| first_heading(text))
                          .or_else(|| fallback_title.map(String::from))
                          .unwrap_or_default();

    // The front matter's *contents* arrive as ordinary `Text` events, so
    // dropping the block's delimiters is not enough: the whole span has to be
    // suppressed, or the YAML is set as the first paragraph of the document.
    let mut in_metadata = false;
    let events = Parser::new_ext(text, options()).filter_map(move |event| {
        match event {
            // Raw HTML cannot be passed through: the parser downstream is an XML
            // parser, and a stray `<br>` or `<details>` would take the whole
            // file down. It is *escaped* rather than dropped, because a raw
            // `<details>` opens a CommonMark HTML block that runs to the next
            // blank line — so dropping it silently eats the prose after it too,
            // which is a worse failure than showing the markup.
            Event::Html(html) | Event::InlineHtml(html) => {
                let trimmed = html.trim();
                if trimmed.starts_with("<!--") {
                    // Comments are meant to be invisible; that much is safe.
                    None
                } else if matches!(trimmed, "<br>" | "<br/>" | "<br />") {
                    Some(Event::Html("<br/>".into()))
                } else {
                    // push_html escapes Text, so this reaches the engine as
                    // literal, well formed character data.
                    Some(Event::Text(html))
                }
            },
            Event::Start(Tag::MetadataBlock(..)) => { in_metadata = true; None },
            Event::End(TagEnd::MetadataBlock(..)) => { in_metadata = false; None },
            _ if in_metadata => None,
            // The engine draws no borders, so an `<hr/>` would be an invisible
            // empty block. Say it in type instead.
            Event::Rule => Some(Event::Html(r#"<p class="rule">* * *</p>"#.into())),
            // Likewise `<input type="checkbox"/>`: nothing would be drawn.
            Event::TaskListMarker(done) => Some(Event::Html(
                if done { "<code>[x]</code> ".into() } else { "<code>[ ]</code> ".into() })),
            event => Some(event),
        }
    });

    let mut body = String::with_capacity(text.len() * 3 / 2);
    html::push_html(&mut body, events);

    let mut buf = String::with_capacity(body.len() + 256);
    buf.push_str("<html>\n\t<head>\n\t\t<title>");
    buf.push_str(&escape(&title));
    buf.push_str("</title>\n");
    if let Some(author) = author {
        buf.push_str("\t\t<meta name=\"author\" content=\"");
        buf.push_str(&escape(&author));
        buf.push_str("\"/>\n");
    }
    buf.push_str("\t</head>\n\t<body>\n");
    buf.push_str(&body);
    buf.push_str("\t</body>\n</html>");
    buf
}

/// Open a `.md` file as an `HtmlDocument`.
///
/// `parent` is the Markdown file's directory, so relative image links resolve
/// the way they do for a real `.html` file.
pub fn open<P: AsRef<Path>>(path: P) -> Result<HtmlDocument, Error> {
    let text = fs::read_to_string(path.as_ref())?;
    let stem = path.as_ref().file_stem().and_then(|s| s.to_str());
    let mut doc = HtmlDocument::new_from_memory(&to_html(&text, stem));
    doc.set_parent(path.as_ref().parent().unwrap_or_else(|| Path::new("")));
    doc.set_viewer_stylesheet(VIEWER_STYLESHEET);
    doc.set_user_stylesheet(USER_STYLESHEET);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_first_heading() {
        let html = to_html("# Notes on *Phaedrus*\n\nBody.\n", Some("phaedrus"));
        assert!(html.contains("<title>Notes on Phaedrus</title>"), "{}", html);
    }

    #[test]
    fn title_from_front_matter_wins() {
        let html = to_html("---\ntitle: Real Title\nauthor: Plato\n---\n\n# Heading\n", None);
        assert!(html.contains("<title>Real Title</title>"), "{}", html);
        assert!(html.contains("name=\"author\" content=\"Plato\""), "{}", html);
        // The YAML's *text* is what leaks, and it leaks as a paragraph, so
        // check the body rather than any particular tag.
        let body = html.split("<body>").nth(1).unwrap();
        assert!(!body.contains("Real Title"), "front matter leaked into the body: {}", html);
        assert!(!body.contains("author:"), "front matter leaked into the body: {}", html);
    }

    #[test]
    fn title_falls_back_to_the_file_stem() {
        let html = to_html("Just a paragraph.\n", Some("scratch"));
        assert!(html.contains("<title>scratch</title>"), "{}", html);
    }

    #[test]
    fn raw_html_is_escaped_not_passed_through() {
        let html = to_html("a <details><summary>x</summary></details> trailing prose\n", None);
        assert!(!html.contains("<details>"), "raw html reached the XML parser: {}", html);
        assert!(html.contains("&lt;details&gt;"), "{}", html);
        // The point of escaping rather than dropping: an HTML block runs to the
        // next blank line, so dropping it would take this text with it.
        assert!(html.contains("trailing prose"), "prose after raw html was eaten: {}", html);
    }

    #[test]
    fn line_breaks_survive_and_comments_do_not() {
        let html = to_html("a <br> b\n\n<!-- a note to self -->\n", None);
        assert!(html.contains("<br/>"), "{}", html);
        assert!(!html.contains("note to self"), "html comment was rendered: {}", html);
    }

    #[test]
    fn tables_and_code_survive() {
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn f() {}\n```\n", None);
        assert!(html.contains("<table>") && html.contains("<th>") && html.contains("<td>"), "{}", html);
        assert!(html.contains("<pre><code"), "{}", html);
    }

    #[test]
    fn ampersands_in_the_title_are_escaped() {
        let html = to_html("# Tom & Jerry <3\n", None);
        assert!(html.contains("<title>Tom &amp; Jerry &lt;3</title>"), "{}", html);
    }
}
