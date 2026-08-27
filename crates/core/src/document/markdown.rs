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

use std::cell::RefCell;
use std::fmt;
use std::fs;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
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

/// The event stream `to_html` renders, with each event's source byte range.
///
/// Synthesized replacements (escaped raw HTML, `<br/>`, the rule and task
/// markers) keep the range of the event they stand in for.
fn filtered_events(text: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> {
    // The front matter's *contents* arrive as ordinary `Text` events, so
    // dropping the block's delimiters is not enough: the whole span has to be
    // suppressed, or the YAML is set as the first paragraph of the document.
    let mut in_metadata = false;
    Parser::new_ext(text, options()).into_offset_iter().filter_map(move |(event, range)| {
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
        }.map(|event| (event, range))
    })
}

/// From XHTML byte offsets — the coin `TextLocation::Dynamic` deals in — back
/// to 1-based lines of the Markdown source.
///
/// Rebuilt on demand from the `.md` file; the mapping is deterministic, so it
/// lines up with offsets recorded by any earlier render of the same source.
pub struct SourceMap {
    /// `(xhtml_offset, source_range)` per rendered event, ascending in both.
    entries: Vec<(usize, Range<usize>)>,
    /// Byte offset of each line start in the source.
    line_starts: Vec<usize>,
}

impl SourceMap {
    fn line_at(&self, md_offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= md_offset)
    }

    /// The source position under an XHTML offset: interpolated within the
    /// covering event, clamped to that event's span. Escaping and smart
    /// punctuation stretch the output relative to the source, so the
    /// interpolation can overshoot within one multi-line event — never past it.
    fn source_offset(&self, xhtml_offset: usize) -> Option<usize> {
        let idx = self.entries.partition_point(|e| e.0 <= xhtml_offset);
        let (event_offset, range) = self.entries.get(idx.checked_sub(1)?)?;
        let interpolated = range.start + (xhtml_offset - event_offset);
        Some(interpolated.min(range.end.saturating_sub(1)).max(range.start))
    }

    /// 1-based source line containing this XHTML byte offset, if mapped.
    pub fn line_of(&self, xhtml_offset: usize) -> Option<usize> {
        self.source_offset(xhtml_offset).map(|o| self.line_at(o))
    }

    /// Inclusive 1-based line range for a selection's `[start, end]` offsets.
    pub fn line_range(&self, sel: [usize; 2]) -> Option<(usize, usize)> {
        let start = self.line_of(sel[0])?;
        let end = self.line_of(sel[1])?;
        Some((start.min(end), start.max(end)))
    }
}

/// A `fmt::Write` over a shared buffer, so the renderer and the offset
/// recorder can watch the same string grow.
struct SharedBuf(Rc<RefCell<String>>);

impl fmt::Write for SharedBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.borrow_mut().push_str(s);
        Ok(())
    }
}

/// Markdown to a self-contained XHTML document, plus the offset map back to
/// the source.
///
/// The document must be byte-identical to what `to_html` has always produced:
/// annotations persist `TextLocation::Dynamic` byte offsets into it. Hence one
/// rendering pass shared with `to_html`, not a parallel one — the map is
/// recorded by snooping on the single `write_html_fmt` writer, because a
/// per-event `push_html` would reset the writer's newline state and drift.
pub fn to_html_with_map(text: &str, fallback_title: Option<&str>) -> (String, SourceMap) {
    let (meta_title, author) = front_matter(text);
    let title = meta_title.or_else(|| first_heading(text))
                          .or_else(|| fallback_title.map(String::from))
                          .unwrap_or_default();

    let body = Rc::new(RefCell::new(String::with_capacity(text.len() * 3 / 2)));
    let entries = Rc::new(RefCell::new(Vec::new()));
    {
        // The writer consumes each event fully before pulling the next, so the
        // buffer's length as an event is yielded is where its output begins.
        let recorder = {
            let body = Rc::clone(&body);
            let entries = Rc::clone(&entries);
            move |(event, range)| {
                entries.borrow_mut().push((body.borrow().len(), range));
                event
            }
        };
        html::write_html_fmt(SharedBuf(Rc::clone(&body)), filtered_events(text).map(recorder)).ok();
    }
    let body = Rc::try_unwrap(body).unwrap().into_inner();
    let mut entries = Rc::try_unwrap(entries).unwrap().into_inner();

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
    for entry in &mut entries {
        entry.0 += buf.len();
    }
    buf.push_str(&body);
    buf.push_str("\t</body>\n</html>");

    let mut line_starts = vec![0];
    line_starts.extend(text.bytes().enumerate()
                           .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)));

    (buf, SourceMap { entries, line_starts })
}

/// Markdown to a self-contained XHTML document.
pub fn to_html(text: &str, fallback_title: Option<&str>) -> String {
    to_html_with_map(text, fallback_title).0
}

/// Highlights as a grep-style listing the Mac can jump around in:
/// `name:42: line` for one-line spans, `name:57-61:` followed by the
/// four-space-indented lines for longer ones.
///
/// `spans` are inclusive 1-based line ranges, sorted, duplicates dropped by
/// the caller. Snippets come from the source lines, not from the rendered
/// excerpt, so what the Mac sees is what the file says.
pub fn format_highlights(name: &str, source: &str, spans: &[(usize, usize)]) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    for &(start, end) in spans {
        if start == 0 || start > lines.len() {
            continue;
        }
        let end = end.min(lines.len()).max(start);
        if start == end {
            out.push_str(&format!("{}:{}: {}\n", name, start, lines[start-1].trim_end()));
        } else {
            out.push_str(&format!("{}:{}-{}:\n", name, start, end));
            for line in &lines[start-1..end] {
                out.push_str("    ");
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
    }
    out
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

    /// The renderer as it was before the offset map existed: plain `Parser`,
    /// the same filtering, one `push_html`. Annotations persist byte offsets
    /// into that output, so the mapped render must reproduce it exactly.
    fn reference_body(text: &str) -> String {
        let mut in_metadata = false;
        let events = Parser::new_ext(text, options()).filter_map(move |event| {
            match event {
                Event::Html(html) | Event::InlineHtml(html) => {
                    let trimmed = html.trim();
                    if trimmed.starts_with("<!--") {
                        None
                    } else if matches!(trimmed, "<br>" | "<br/>" | "<br />") {
                        Some(Event::Html("<br/>".into()))
                    } else {
                        Some(Event::Text(html))
                    }
                },
                Event::Start(Tag::MetadataBlock(..)) => { in_metadata = true; None },
                Event::End(TagEnd::MetadataBlock(..)) => { in_metadata = false; None },
                _ if in_metadata => None,
                Event::Rule => Some(Event::Html(r#"<p class="rule">* * *</p>"#.into())),
                Event::TaskListMarker(done) => Some(Event::Html(
                    if done { "<code>[x]</code> ".into() } else { "<code>[ ]</code> ".into() })),
                event => Some(event),
            }
        });
        let mut body = String::new();
        html::push_html(&mut body, events);
        body
    }

    const SAMPLE: &str = "\
---
title: Sample
---

# Heading

First paragraph, one line.

A paragraph that goes on
over three source lines
before it finally ends.

- item one
- item two

```rust
fn f() {}
let x = 1;
```

Last paragraph mentions kumquats.
";

    #[test]
    fn mapped_render_is_byte_identical_to_the_reference() {
        for text in [SAMPLE,
                     "a <br> b\n\n<!-- gone -->\n\n---\n\n- [x] done\n- [ ] not\n",
                     "raw <details>x</details> tail\n\n| a |\n|---|\n| 1 |\n"] {
            let (html, _) = to_html_with_map(text, Some("t"));
            let body = reference_body(text);
            let inner = html.split("<body>\n").nth(1).unwrap()
                            .strip_suffix("\t</body>\n</html>").unwrap();
            assert_eq!(inner, body, "mapped render drifted from push_html for {:?}", text);
        }
    }

    #[test]
    fn map_locates_source_lines() {
        let (html, map) = to_html_with_map(SAMPLE, None);
        for (needle, line) in [("Heading", 5), ("First", 7), ("finally", 11),
                               ("item two", 14), ("kumquats", 21)] {
            let offset = html.find(needle).unwrap();
            assert_eq!(map.line_of(offset), Some(line), "{}", needle);
        }
    }

    #[test]
    fn line_range_spans_a_paragraph() {
        let (html, map) = to_html_with_map(SAMPLE, None);
        let start = html.find("goes").unwrap();
        let end = html.find("ends").unwrap();
        assert_eq!(map.line_range([start, end]), Some((9, 11)));
        let word = html.find("kumquats").unwrap();
        assert_eq!(map.line_range([word, word]), Some((21, 21)));
    }

    #[test]
    fn unmapped_offsets_resolve_to_nothing() {
        let (_, map) = to_html_with_map(SAMPLE, None);
        assert_eq!(map.line_of(0), None, "the preamble is not source");
    }

    #[test]
    fn highlights_format_single_and_multi_line() {
        let source = "one\ntwo\nthree\nfour\nfive\n";
        let out = format_highlights("plan.md", source, &[(2, 2), (3, 5)]);
        assert_eq!(out, "plan.md:2: two\n\
                         plan.md:3-5:\n    three\n    four\n    five\n");
    }

    #[test]
    fn highlights_format_clamps_out_of_range_spans() {
        let out = format_highlights("plan.md", "only\n", &[(1, 9), (7, 8)]);
        assert_eq!(out, "plan.md:1: only\n");
    }
}
