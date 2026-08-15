//! Turn a source's HTML fragment into something `document::html` can parse.
//!
//! Both sources hand us small islands of HTML written by someone else: Hacker
//! News comment bodies, and the `description`/`summary` of a feed entry. Two
//! properties have to hold before that can reach [`XmlParser`], which is a
//! *XML* parser with no implicit closing and no void-element table:
//!
//! 1. **Every element closes.** Measured on a real HN item page: `<p>` occurs
//!    40 times and `</p>` zero times -- HN uses `<p>` as a separator, the way
//!    HTML lets you. Unclosed, the parser nests every following paragraph one
//!    level deeper.
//! 2. **Nothing unexpected gets through.** The vocabulary inside HN comment
//!    bodies is exactly `a, code, i, p, pre` (checked across a 724-comment
//!    thread); feeds are looser. Rather than trust either, this keeps an
//!    allowlist and drops the rest -- which also means no `<script>`, no
//!    styling and no remote `<img>` can arrive from a stranger's comment.
//!
//! Text is passed through as-is: it arrives already escaped from both sources,
//! and `decode_entities` runs later during layout. The one exception is a
//! stray `&` or `<` that is not part of an entity or a tag, which is escaped so
//! it cannot truncate the document.

use std::fmt::Write;

/// Elements worth keeping. Everything outside this list is dropped *while
/// keeping its children* -- a `<div>` wrapper in a feed blurb should not take
/// the blurb with it.
const ALLOWED: &[&str] = &[
    "a", "b", "blockquote", "code", "em", "i", "li", "ol", "p", "pre", "strong", "ul",
];

/// Kept, but as empty elements. `img` is deliberately *not* here: nothing in
/// this reader fetches remote images, and a blurb full of tracking pixels is
/// exactly what an e-ink reader does not need.
const VOID: &[&str] = &["br"];

/// Block-level elements. An open `<p>` closes when any of these starts, which
/// is HTML's own rule and the whole of the implicit-close story we need: HN
/// writes `first<p>second<p><pre>code</pre>`, and without this the `<pre>`
/// would end up inside the paragraph.
const BLOCKS: &[&str] = &["blockquote", "li", "ol", "p", "pre", "ul"];

/// Elements whose *contents* go too, in a blurb. A feed that ships article
/// HTML ships its image captions with it, and since no image is drawn here the
/// caption arrives as a non-sequitur at the top of the blurb -- The Verge opens
/// every entry with "…pop music. | Image: Daniel Randall".
const DROPPED_WHOLE: &[&str] = &["caption", "figcaption", "figure", "script", "style"];

/// An open element, and where its tag sits in the output -- so that an element
/// that turns out to contain nothing can be removed again rather than left as
/// an empty `<p></p>` between two paragraphs.
struct Open {
    name: &'static str,
    tag_start: usize,
    content_start: usize,
}

pub fn sanitize_fragment(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut open: Vec<Open> = Vec::new();
    let mut rest = input;

    while let Some(lt) = rest.find('<') {
        push_text(&mut out, &rest[..lt]);
        rest = &rest[lt..];

        match parse_tag(rest) {
            Some((tag, closing, after)) => {
                rest = after;
                if let Some(name) = allowed_name(tag.name) {
                    if closing {
                        close_through(&mut out, &mut open, name);
                    } else if VOID.contains(&name) {
                        out.push_str("<br/>");
                    } else {
                        if BLOCKS.contains(&name) {
                            if let Some(top) = open.last().map(|o| o.name) {
                                if top == "p" || (top == name && name == "li") {
                                    close_through(&mut out, &mut open, top);
                                }
                            }
                        }
                        let tag_start = out.len();
                        let _ = write!(out, "<{name}");
                        if name == "a" {
                            if let Some(href) = tag.href {
                                // The href arrives already escaped, from HTML.
                                // Escaping it again turns `&amp;` in a query
                                // string into a visible `&amp;amp;` -- and a
                                // wrong link.
                                out.push_str(" href=\"");
                                push_attribute(&mut out, href);
                                out.push('"');
                            }
                        }
                        out.push('>');
                        open.push(Open { name, tag_start, content_start: out.len() });
                    }
                }
                // An element outside the allowlist: the tag disappears, its
                // children do not.
            }
            // A bare `<` -- a comment that talks about `a < b`, which HN would
            // normally have escaped but a feed might not.
            None => {
                out.push_str("&lt;");
                rest = &rest[1..];
            }
        }
    }

    push_text(&mut out, rest);

    while let Some(item) = open.pop() {
        close_one(&mut out, item);
    }

    out
}

/// Emit the closing tag -- or, if nothing was written since the opening one,
/// take the opening tag back. HN's `first<p>second<p><pre>…` would otherwise
/// leave an empty paragraph where the second `<p>` was closed by the `<pre>`.
fn close_one(out: &mut String, item: Open) {
    if out.len() == item.content_start {
        out.truncate(item.tag_start);
    } else {
        let _ = write!(out, "</{}>", item.name);
    }
}

struct Tag<'a> {
    name: &'a str,
    href: Option<&'a str>,
}

/// Parse one tag at the start of `input`. Returns the tag, whether it was a
/// closing tag, and the rest of the input. `None` means "this `<` does not
/// begin a tag", including the comment and doctype cases, which are skipped
/// wholesale by the caller only if they parse -- so they fall through to being
/// escaped, which is the safe direction.
fn parse_tag(input: &str) -> Option<(Tag<'_>, bool, &str)> {
    let body = input.strip_prefix('<')?;
    let (closing, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let name_len = body.find(|c: char| !c.is_ascii_alphanumeric())
                       .unwrap_or(body.len());
    if name_len == 0 {
        return None;
    }
    let name = &body[..name_len];
    let rest = &body[name_len..];
    let end = rest.find('>')?;
    let attrs = &rest[..end];
    Some((Tag { name, href: (!closing).then(|| href_of(attrs)).flatten() },
          closing,
          &rest[end + 1..]))
}

/// The one attribute that survives. Quoted values only -- an unquoted `href`
/// has never appeared in either source, and guessing where one ends is how
/// sanitizers grow holes.
fn href_of(attrs: &str) -> Option<&str> {
    let at = attrs.find("href")?;
    let rest = attrs[at + 4..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

/// Match case-insensitively but return the canonical spelling, so the output
/// is always lowercase however the input was written.
fn allowed_name(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    ALLOWED.iter().chain(VOID).find(|&&a| a == lower).copied()
}

/// Close `name`, and anything opened inside it that never closed. Without the
/// "and anything inside it" part, one stray `<i>` in a comment would swallow
/// the rest of the thread.
fn close_through(out: &mut String, open: &mut Vec<Open>, name: &str) {
    if !open.iter().any(|o| o.name == name) {
        return;                     // a close with no open: drop it
    }
    while let Some(item) = open.pop() {
        let done = item.name == name;
        close_one(out, item);
        if done {
            break;
        }
    }
}

/// Text passes through, except for a `&` that is not the start of an entity.
/// Both sources escape their entities correctly today; this is what keeps a
/// day when one of them doesn't from producing a broken document.
fn push_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        if is_entity(rest) {
            let end = rest.find(';').unwrap() + 1;
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        } else {
            out.push_str("&amp;");
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
}

/// An attribute value that came out of HTML: entities kept as they are, the
/// three characters that could end the attribute or the tag escaped.
fn push_attribute(out: &mut String, value: &str) {
    let mut escaped = String::with_capacity(value.len());
    push_text(&mut escaped, value);
    for c in escaped.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn is_entity(text: &str) -> bool {
    let body = &text[1..];
    let end = match body.find(';') {
        Some(end) if end > 0 && end <= 10 => end,
        _ => return false,
    };
    let name = &body[..end];
    match name.strip_prefix('#') {
        Some(num) => match num.strip_prefix(['x', 'X']) {
            Some(hex) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
            None => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        },
        None => name.chars().all(|c| c.is_ascii_alphanumeric()),
    }
}

/// The text of a fragment, with the markup dropped and the whitespace
/// collapsed, cut to about `max_chars`.
///
/// This is what a feed's blurb becomes. A blurb is for skimming, and some feeds
/// ship the whole article in it -- one Verge entry filled the screen -- so the
/// list would become a list of one. Dropping the markup rather than keeping a
/// truncated subset of it also sidesteps the awkward part of cutting HTML
/// short: there is no half-open element to close, because there are no
/// elements.
///
/// Entities are left encoded, as everywhere else here: the layout engine
/// decodes them. Cutting happens at a word boundary, which is also what keeps
/// an entity from being cut in half -- at the cost of `&#x27;` counting as six
/// characters rather than one, so a blurb thick with entities is cut a little
/// early. Not worth decoding and re-encoding a blurb to fix.
pub fn text_only(input: &str, max_chars: usize) -> String {
    let mut text = String::with_capacity(input.len().min(max_chars * 2));
    let mut rest = input;

    // Same scan as `sanitize_fragment`, keeping only what is between the tags.
    while let Some(lt) = rest.find('<') {
        push_text(&mut text, &rest[..lt]);
        rest = &rest[lt..];
        match parse_tag(rest) {
            Some((tag, closing, after)) => {
                let lower = tag.name.to_ascii_lowercase();
                if !closing && DROPPED_WHOLE.contains(&lower.as_str()) {
                    // Skip to the matching close, or to the end if the feed
                    // never wrote one.
                    let close = format!("</{lower}");
                    rest = match after.find(&close).and_then(|at| {
                        after[at..].find('>').map(|end| &after[at + end + 1..])
                    }) {
                        Some(after_close) => after_close,
                        None => "",
                    };
                    text.push(' ');
                    continue;
                }
                // A dropped *block* element must not glue two sentences
                // together -- "…must-listen podcast.Sloan is a composer…" --
                // but a dropped inline one must not split a sentence either:
                // "with a link ." for a closing `</a>`.
                if BLOCKS.contains(&lower.as_str()) || VOID.contains(&lower.as_str()) {
                    text.push(' ');
                }
                rest = after;
            }
            None => {
                text.push_str("&lt;");
                rest = &rest[1..];
            }
        }
    }
    push_text(&mut text, rest);

    let mut out = String::with_capacity(text.len().min(max_chars + 8));
    let mut count = 0;
    let mut truncated = false;
    for word in text.split_whitespace() {
        // `chars().count()` and not `len()`: a cut measured in bytes would land
        // differently for the same sentence in another language.
        let width = word.chars().count();
        if count > 0 && count + 1 + width > max_chars {
            truncated = true;
            break;
        }
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        out.push_str(word);
        count += width;
    }

    if truncated {
        // A trailing comma before an ellipsis reads as a typo.
        while out.ends_with([',', ';', ':', '-', '—', '–']) {
            out.pop();
        }
        out.push('…');
    }
    out
}

pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn escape_attribute(text: &str) -> String {
    escape_text(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hn_paragraph_separators_become_real_paragraphs() {
        // The shape every HN comment body has: <p> as a separator, never closed.
        let out = sanitize_fragment("first<p>second<p>third");
        assert_eq!(out, "first<p>second</p><p>third</p>");
    }

    #[test]
    fn the_hn_vocabulary_survives_intact() {
        let input = "see <a href=\"https://example.com/x?a=1&amp;b=2\" rel=\"nofollow\">this</a> \
                     and <i>that</i><p><pre><code>fn main() {}</code></pre>";
        // The `<p>` before the `<pre>` opens nothing and closes immediately,
        // so it leaves no empty paragraph behind; the href keeps its single
        // `&amp;` rather than being escaped a second time.
        assert_eq!(sanitize_fragment(input),
                   "see <a href=\"https://example.com/x?a=1&amp;b=2\">this</a> \
                    and <i>that</i><pre><code>fn main() {}</code></pre>");
    }

    #[test]
    fn disallowed_elements_lose_the_tag_and_keep_the_text() {
        assert_eq!(sanitize_fragment("<div class=\"x\">kept</div>"), "kept");
        assert_eq!(sanitize_fragment("<script>alert(1)</script>"), "alert(1)");
        assert_eq!(sanitize_fragment("<img src=\"http://tracker/x.gif\">no pixels"),
                   "no pixels");
    }

    #[test]
    fn attributes_other_than_href_are_dropped() {
        assert_eq!(sanitize_fragment("<a href=\"x\" onclick=\"boom()\" style=\"color:red\">t</a>"),
                   "<a href=\"x\">t</a>");
    }

    #[test]
    fn unclosed_and_unbalanced_elements_cannot_escape_the_fragment() {
        assert_eq!(sanitize_fragment("<i>dangling"), "<i>dangling</i>");
        assert_eq!(sanitize_fragment("</i>orphan"), "orphan");
        assert_eq!(sanitize_fragment("<b><i>x</b>y"), "<b><i>x</i></b>y");
    }

    #[test]
    fn br_is_closed_and_a_bare_less_than_is_escaped() {
        assert_eq!(sanitize_fragment("a<br>b"), "a<br/>b");
        assert_eq!(sanitize_fragment("if a < b then"), "if a &lt; b then");
    }

    #[test]
    fn entities_pass_through_but_a_bare_ampersand_is_escaped() {
        assert_eq!(sanitize_fragment("&amp; &#x27; &nbsp; &#8212;"),
                   "&amp; &#x27; &nbsp; &#8212;");
        assert_eq!(sanitize_fragment("Q&A"), "Q&amp;A");
    }

    #[test]
    fn a_blurb_becomes_a_few_lines_of_text() {
        let blurb = "<p>They&#x27;re the <b>Lennon</b> of pop.</p>\
                     <p>As if you needed more reason to love it, it is also, \
                     according to lore, the genesis for the podcast.</p>";
        // Markup gone, entity kept for the layout engine, sentences not glued
        // together where a block element was dropped.
        assert_eq!(text_only(blurb, 60),
                   "They&#x27;re the Lennon of pop. As if you needed more reason…");
        // Under the limit, nothing is added.
        assert_eq!(text_only("<p>Short.</p>", 60), "Short.");
    }

    /// The measured case: The Verge's `content` opens with the hero image's
    /// caption, which without this reads as the first sentence of the story.
    #[test]
    fn an_image_caption_is_not_the_blurb() {
        let blurb = "<figure><img src=\"x.jpg\"/><figcaption>They&#x27;re the Lennon. \
                     | Image: Daniel Randall</figcaption></figure>\
                     <p>As if you needed more reason to love it.</p>";
        assert_eq!(text_only(blurb, 200), "As if you needed more reason to love it.");
    }

    #[test]
    fn truncation_cuts_at_a_word_and_tidies_the_join() {
        assert_eq!(text_only("one two three four", 9), "one two…");
        // Never mid-entity: the cut is on whitespace, and entities have none.
        assert!(!text_only("a &amp; b &amp; c &amp; d", 12).contains("&am…"));
        // A dangling comma before the ellipsis reads as a typo.
        assert_eq!(text_only("first, second", 8), "first…");
    }

    #[test]
    fn the_output_always_parses_as_xml() {
        // The property that matters: whatever goes in, `XmlParser` gets a tree
        // back with the text still in it.
        use crate::document::html::xml::XmlParser;
        let messy = "a < b <p>see <a href='u'>x<i>y</b></a><div><br>end";
        let clean = format!("<div>{}</div>", sanitize_fragment(messy));
        let tree = XmlParser::new(&clean).parse();
        assert!(tree.root().text().contains("end"));
    }
}
