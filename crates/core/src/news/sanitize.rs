//! Turn a source's HTML fragment into something `document::html` can parse.
//!
//! Every source hands us HTML written by someone else: Hacker News comment
//! bodies, the `description`/`summary` of a feed entry, and -- since articles
//! opened in-reader -- whatever the readability extractor pulled out of an
//! arbitrary page. Two properties have to hold before any of that can reach
//! [`XmlParser`], which is a *XML* parser with no implicit closing and no
//! void-element table:
//!
//! 1. **Every element closes.** Measured on a real HN item page: `<p>` occurs
//!    40 times and `</p>` zero times -- HN uses `<p>` as a separator, the way
//!    HTML lets you. Unclosed, the parser nests every following paragraph one
//!    level deeper.
//! 2. **Nothing unexpected gets through.** The vocabulary began as HN's --
//!    comment bodies contain exactly `a, code, i, p, pre`, checked across a
//!    724-comment thread -- and grew the headings and `hr` an article needs
//!    when whole articles started arriving. Feeds and extracted articles are
//!    looser than HN; rather than trust any of them, this keeps an allowlist
//!    and drops the rest -- which also means no `<script>`, no styling and no
//!    remote `<img>` can arrive from a stranger's page. A few elements
//!    ([`DROPPED_WHOLE`]) take their contents with them: a script body or an
//!    image caption must not surface as article text.
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
    "a", "b", "blockquote", "code", "em", "h1", "h2", "h3", "h4", "h5", "h6",
    "i", "li", "ol", "p", "pre", "strong", "ul",
];

/// Kept, but as empty elements. `img` is deliberately *not* here: a comment
/// or a blurb is a stranger's fragment shown unasked, and a blurb full of
/// tracking pixels is exactly what an e-ink reader does not need. An article
/// the reader chose to open is the one exception, and it goes through
/// [`sanitize_article_fragment`] instead.
const VOID: &[&str] = &["br", "hr"];

/// Block-level elements. An open `<p>` closes when any of these starts, which
/// is HTML's own rule and the whole of the implicit-close story we need: HN
/// writes `first<p>second<p><pre>code</pre>`, and without this the `<pre>`
/// would end up inside the paragraph.
const BLOCKS: &[&str] = &[
    "blockquote", "figcaption", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "li", "ol",
    "p", "pre", "ul",
];

/// Elements whose *contents* go too, everywhere. `script` is the sharp case:
/// its body is code, and dropping only the tags would print it as prose.
const DROPPED_WHOLE: &[&str] = &["caption", "script", "style"];

/// Dropped whole only where no image is drawn -- comments and blurbs. There a
/// caption is a non-sequitur: The Verge opens every entry's `content` with
/// "…pop music. | Image: Daniel Randall". In an article the image *is* drawn
/// (readability wraps most of them in `<figure>`), so the figure unwraps like
/// any other container and its caption survives as a `figcaption` for the
/// stylesheet to set under the picture.
const FIGURES: &[&str] = &["figcaption", "figure"];

/// An open element, and where its tag sits in the output -- so that an element
/// that turns out to contain nothing can be removed again rather than left as
/// an empty `<p></p>` between two paragraphs.
struct Open {
    name: &'static str,
    tag_start: usize,
    content_start: usize,
}

pub fn sanitize_fragment(input: &str) -> String {
    sanitize(input, false)
}

/// The article vocabulary: everything `sanitize_fragment` keeps, plus `img`.
///
/// The split exists because the tracking-pixel rationale in [`VOID`] is about
/// *whose* fragment this is. A stranger's comment or a feed's blurb must not
/// be able to make this reader phone home, so their `img` still vanishes. An
/// article is different: the reader deliberately chose to open that page, its
/// images are part of what was asked for, and the fetching happens once, up
/// front, through the same injected client as the page itself -- not at
/// render time on someone else's schedule.
pub fn sanitize_article_fragment(input: &str) -> String {
    sanitize(input, true)
}

fn sanitize(input: &str, allow_img: bool) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut open: Vec<Open> = Vec::new();
    let mut rest = input;

    while let Some(lt) = rest.find('<') {
        push_text(&mut out, &rest[..lt]);
        rest = &rest[lt..];

        match parse_tag(rest) {
            Some((tag, closing, after)) => {
                let lower = tag.name.to_ascii_lowercase();
                if !closing && (DROPPED_WHOLE.contains(&lower.as_str()) ||
                                (!allow_img && FIGURES.contains(&lower.as_str()))) {
                    rest = skip_dropped(&lower, after);
                    continue;
                }
                rest = after;
                if !closing && lower == "img" {
                    // `img` is void, so there is nothing to keep open and no
                    // children to preserve; either it is emitted whole here or
                    // it vanishes. Only a web URL survives -- the extractor
                    // has already absolutized its srcs, so a `data:` URI or a
                    // relative leftover is a src this reader will never fetch,
                    // and a tag pointing nowhere is better dropped. Every
                    // other attribute (onerror, width, ...) goes.
                    if allow_img {
                        if let Some(src) = tag.src.filter(|src| src.starts_with("https://") ||
                                                                src.starts_with("http://")) {
                            out.push_str("<img src=\"");
                            push_attribute(&mut out, src);
                            out.push_str("\"/>");
                        }
                    }
                    continue;
                }
                if let Some(name) = allowed_name(tag.name, allow_img) {
                    if closing {
                        close_through(&mut out, &mut open, name);
                    } else if VOID.contains(&name) {
                        if BLOCKS.contains(&name) {
                            close_implicit(&mut out, &mut open, name);
                        }
                        let _ = write!(out, "<{name}/>");
                    } else {
                        if BLOCKS.contains(&name) {
                            close_implicit(&mut out, &mut open, name);
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
    src: Option<&'a str>,
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
    Some((Tag { name,
                href: (!closing).then(|| quoted_attr(attrs, "href")).flatten(),
                src: (!closing).then(|| quoted_attr(attrs, "src")).flatten() },
          closing,
          &rest[end + 1..]))
}

/// The two attributes that survive anywhere (`href` on a link, `src` on an
/// article's `img`). Quoted values only -- an unquoted value has never
/// appeared in any source, and guessing where one ends is how sanitizers grow
/// holes. The boundary check exists for `src`: lazy-loading markup is full of
/// `data-src`, and matching inside it would resurrect exactly the deferred
/// image URL the site did not put in `src`.
fn quoted_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    loop {
        let at = from + attrs[from..].find(name)?;
        from = at + name.len();
        if at > 0 && attrs[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            continue;
        }
        let rest = attrs[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|&c| c == '"' || c == '\'') else { continue };
        let rest = &rest[1..];
        let end = rest.find(quote)?;
        return Some(&rest[..end]);
    }
}

/// Match case-insensitively but return the canonical spelling, so the output
/// is always lowercase however the input was written. `figcaption` is only a
/// word in the article vocabulary; in a comment or blurb the whole figure was
/// skipped before this is ever asked.
fn allowed_name(name: &str, allow_img: bool) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if allow_img && lower == "figcaption" {
        return Some("figcaption");
    }
    ALLOWED.iter().chain(VOID).find(|&&a| a == lower).copied()
}

/// HTML's implicit-close rule, or the slice of it this vocabulary needs: a
/// starting block closes an open `<p>`, and a new `<li>` closes the previous
/// one.
fn close_implicit(out: &mut String, open: &mut Vec<Open>, name: &str) {
    if let Some(top) = open.last().map(|o| o.name) {
        if top == "p" || (top == name && name == "li") {
            close_through(out, open, top);
        }
    }
}

/// Everything up to and including `</name>` -- or nothing at all, when the
/// close never comes. An unclosed `<figure>` therefore swallows the rest of
/// the input rather than letting its caption pose as body text, which is the
/// safe direction: markup broken enough to leave a dropped element open is
/// markup whose remainder cannot be told apart from that element's contents.
fn skip_dropped<'a>(name: &str, after: &'a str) -> &'a str {
    let close = format!("</{name}");
    after.find(&close)
         .and_then(|at| after[at..].find('>').map(|end| &after[at + end + 1..]))
         .unwrap_or("")
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
                // A blurb draws no image, so figures go whole here, always.
                if !closing && (DROPPED_WHOLE.contains(&lower.as_str()) ||
                                FIGURES.contains(&lower.as_str())) {
                    rest = skip_dropped(&lower, after);
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
        assert_eq!(sanitize_fragment("<img src=\"http://tracker/x.gif\">no pixels"),
                   "no pixels");
    }

    /// A `<script>` body is code, not prose: dropping only the tags would
    /// print it. It goes whole, like everything in `DROPPED_WHOLE`.
    #[test]
    fn a_script_vanishes_contents_included() {
        assert_eq!(sanitize_fragment("before<script>alert(1)</script>after"),
                   "beforeafter");
        assert_eq!(sanitize_fragment("<style>p { display: none }</style>text"), "text");
    }

    #[test]
    fn article_headings_survive_and_close_an_open_paragraph() {
        assert_eq!(sanitize_fragment("<h2>Section</h2><p>body</p>"),
                   "<h2>Section</h2><p>body</p>");
        // Readability output can leave a `<p>` open before a heading; HTML's
        // own rule is that the heading closes it.
        assert_eq!(sanitize_fragment("<p>intro<h3>Next</h3>"),
                   "<p>intro</p><h3>Next</h3>");
    }

    #[test]
    fn hr_is_closed_like_br() {
        assert_eq!(sanitize_fragment("one<hr>two"), "one<hr/>two");
        assert_eq!(sanitize_fragment("<p>one<hr>two"), "<p>one</p><hr/>two");
    }

    #[test]
    fn a_figure_and_its_caption_vanish_from_a_fragment() {
        let input = "<p>seen</p><figure><img src=\"x.jpg\"/>\
                     <figcaption>Photo: A. Nobody</figcaption></figure><p>also seen</p>";
        assert_eq!(sanitize_fragment(input), "<p>seen</p><p>also seen</p>");
    }

    /// The documented trade in `skip_dropped`: an unclosed dropped element
    /// truncates the rest of the input instead of letting its contents leak.
    #[test]
    fn an_unclosed_figure_swallows_what_follows_rather_than_leaking_its_caption() {
        assert_eq!(sanitize_fragment("<p>seen</p><figure><figcaption>leak?"),
                   "<p>seen</p>");
    }

    /// In an article the image is drawn, so the figure that readability wraps
    /// around most images unwraps like any container, and its caption stays --
    /// as a `figcaption` the stylesheet can set under the picture.
    #[test]
    fn an_article_figure_unwraps_and_keeps_its_image_and_caption() {
        let input = "<figure><img src=\"https://example.com/x.jpg\">\
                     <figcaption>Photo: A. Somebody</figcaption></figure>";
        assert_eq!(sanitize_article_fragment(input),
                   "<img src=\"https://example.com/x.jpg\"/>\
                    <figcaption>Photo: A. Somebody</figcaption>");
    }

    /// The article exception: an `img` survives, reduced to its `src`. The
    /// comment/blurb entry point must keep dropping it -- that difference is
    /// the whole reason two entry points exist.
    #[test]
    fn img_survives_only_in_the_article_vocabulary() {
        let input = "<p>a</p><img src=\"https://example.com/x.png\" width=\"600\" \
                     onerror=\"boom()\" loading=\"lazy\">";
        assert_eq!(sanitize_article_fragment(input),
                   "<p>a</p><img src=\"https://example.com/x.png\"/>");
        assert_eq!(sanitize_fragment(input), "<p>a</p>");
    }

    /// The extractor absolutizes srcs, so anything that is not a web URL --
    /// a `data:` URI, a relative leftover, no src at all -- is a src this
    /// reader will never fetch, and the tag goes with it.
    #[test]
    fn a_non_web_src_drops_the_img_entirely() {
        assert_eq!(sanitize_article_fragment("<img src=\"data:image/png;base64,AAAA\">kept"),
                   "kept");
        assert_eq!(sanitize_article_fragment("<img src=\"/logo.png\">kept"), "kept");
        assert_eq!(sanitize_article_fragment("<img alt=\"no src\">kept"), "kept");
        // `data-src` is not `src`: resurrecting a lazy-loader's deferred URL
        // would fetch something the page's own `src` did not name.
        assert_eq!(sanitize_article_fragment("<img data-src=\"https://example.com/x.png\">kept"),
                   "kept");
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
