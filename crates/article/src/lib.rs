//! Readability extraction: the raw bytes of any web page in, one article out.
//!
//! This is the one place the reader meets a *stranger's* page layout. The
//! `news` sources in core stay superlight because every site there returns
//! structured data; a link on one of those pages leads to arbitrary HTML, and
//! turning that into something worth laying out is exactly the problem
//! readability.js has spent a decade tuning heuristics for. `dom_smoothie` is
//! a close port of it, actively maintained, with html5ever underneath -- so
//! parsing broken markup is the WHATWG error-recovery algorithm, not something
//! to hand-roll, and the scoring that separates an article from its nav bars
//! is Mozilla's, not ours.
//!
//! The crate is separate from `plato-core` for the same reason `plato-net`
//! is: core stays free of html5ever and its tree of parser crates, the way it
//! stays free of rustls, and the front ends link both halves together. What
//! comes out of [`extract`] is *untrusted* HTML -- the extractor's idea of the
//! article, still written in the site's own tags -- and core's
//! `sanitize_fragment` is what makes it trustworthy before layout.
//!
//! Pure function of bytes: no network, no I/O, so every test here runs on
//! hand-written fixtures.

mod flatten;

use std::borrow::Cow;

use anyhow::{bail, format_err, Error};
use dom_smoothie::{Config, Readability};
use encoding_rs::Encoding;

/// The extractor's answer: metadata worth a head block, and the article body
/// as HTML that has been found but not yet made trustworthy.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: String,
    pub byline: Option<String>,
    pub site: Option<String>,
    pub html: String,
}

/// Extract the readable article from a page's raw bytes. `url` must be the
/// absolute URL the bytes came from: it is what turns the page's relative
/// hrefs into absolute ones, so links in the article still lead somewhere
/// after the page is long gone from context.
pub fn extract(raw: &[u8], url: &str) -> Result<Extracted, Error> {
    let html = decode(raw);
    // Grid scaffolding hides parts of an article from readability's
    // sibling-joining (see `flatten`); take it down before scoring.
    let html = match flatten::flatten_grid(html.as_ref()) {
        Some(flat) => Cow::Owned(flat),
        None => html,
    };

    let mut readability = Readability::new(html.as_ref(), Some(url), Some(Config::default()))
        .map_err(|e| format_err!("{url}: {e}"))?;

    // The pre-parse readability check is what rejects a login page or a bare
    // link list: lots of markup, no run of text that scores like prose. Doing
    // it before `parse` matters -- parse will happily return the "best" of a
    // page that has no article in it.
    if !readability.is_probably_readable() {
        bail!("no readable article found");
    }

    let article = readability.parse()
                             .map_err(|_| format_err!("no readable article found"))?;

    if article.text_content.trim().is_empty() {
        bail!("no readable article found");
    }

    Ok(Extracted {
        title: article.title,
        byline: article.byline,
        site: article.site_name,
        html: article.content.to_string(),
    })
}

/// How much of the head to sniff for a `<meta charset>`. The WHATWG
/// prescan uses the same figure, and every page that declares a charset at
/// all declares it well inside the first kilobyte.
const SNIFF_LEN: usize = 1024;

/// Bytes to text, best effort: a BOM wins, then a `<meta>` declaration in the
/// first kilobyte, then UTF-8 with replacement characters -- which is also the
/// right reading of a mislabelled page, since the alternative is refusing it.
fn decode(raw: &[u8]) -> Cow<'_, str> {
    if let Some((encoding, bom_len)) = Encoding::for_bom(raw) {
        let (text, _) = encoding.decode_without_bom_handling(&raw[bom_len..]);
        return text;
    }
    if let Some(encoding) = sniff_meta_charset(&raw[..raw.len().min(SNIFF_LEN)]) {
        let (text, _) = encoding.decode_without_bom_handling(raw);
        return text;
    }
    String::from_utf8_lossy(raw)
}

/// Find `charset=...` in the head, whichever of its two spellings the page
/// uses: `<meta charset=utf-8>` or `<meta http-equiv="content-type"
/// content="text/html; charset=windows-1252">`. Both reduce to the same
/// `charset=` substring, so one case-insensitive scan covers both, quoted or
/// not. `Encoding::for_label` knows every alias the web actually uses.
fn sniff_meta_charset(head: &[u8]) -> Option<&'static Encoding> {
    fn skip(bytes: &[u8], accept: impl Fn(u8) -> bool) -> &[u8] {
        let n = bytes.iter().take_while(|&&b| accept(b)).count();
        &bytes[n..]
    }
    let whitespace = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');

    let lower = head.to_ascii_lowercase();
    let mut rest: &[u8] = &lower;
    while let Some(at) = rest.windows(7).position(|w| w == b"charset") {
        rest = &rest[at + 7..];
        let after = skip(rest, whitespace);
        if after.first() != Some(&b'=') {
            continue;
        }
        let value = skip(&after[1..], |b| whitespace(b) || b == b'"' || b == b'\'');
        let end = value.iter()
                       .position(|&b| whitespace(b) || matches!(b, b'"' | b'\'' | b';'
                                                                   | b'>' | b'/'))
                       .unwrap_or(value.len());
        if let Some(encoding) = Encoding::for_label(&value[..end]) {
            return Some(encoding);
        }
    }
    None
}

/// The `plato-core` side of the join, behind the `core-client` feature,
/// mirroring `plato-net`: core declares what it needs from a readability
/// engine as a trait and stays free of html5ever, this crate answers it and
/// stays free of MuPDF, and the front ends link both.
#[cfg(feature = "core-client")]
pub mod client {
    use plato_core::anyhow::Error;
    use plato_core::news::{ArticleExtractor, ExtractedArticle};

    pub struct Readability;

    impl ArticleExtractor for Readability {
        fn extract(&self, raw: &[u8], url: &str) -> Result<ExtractedArticle, Error> {
            let article = super::extract(raw, url)?;
            Ok(ExtractedArticle {
                title: article.title,
                byline: article.byline,
                site: article.site,
                html: article.html,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page with everything a real one drags along -- head noise, nav,
    /// sidebar, footer -- around an article long enough to score as prose.
    /// The paragraphs are padded on purpose: readability's whole premise is
    /// that articles have runs of text, and a fixture of one-line paragraphs
    /// would be rejected as the link farm it resembles.
    fn realistic_page() -> String {
        let p = "The quick brown fox jumps over the lazy dog, and keeps \
                 jumping for long enough that a readability score built on \
                 commas and character counts recognizes this paragraph as \
                 prose rather than as navigation chrome or boilerplate.";
        format!(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width">
<meta property="og:site_name" content="Example Journal">
<meta property="og:title" content="The Fox Report">
<title>The Fox Report - Example Journal</title>
<script>window.tracker = "please no";</script>
<style>body {{ font-family: sans-serif; }}</style>
</head>
<body>
<nav><a href="/home">Home</a> <a href="/about">About</a> <a href="/login">Log in</a></nav>
<div class="sidebar"><h3>Trending</h3><a href="/1">One weird trick</a><a href="/2">Another</a></div>
<article>
<h1>The Fox Report</h1>
<div class="section-one"><p>{p}</p><p>{p}</p></div>
<div class="section-two"><p>{p}</p><p>See <a href="/methodology">our methodology</a> for details.</p><p>{p}</p></div>
</article>
<footer>Copyright 2026 Example Journal. All rights reserved. Subscribe to the newsletter.</footer>
</body>
</html>"#)
    }

    #[test]
    fn a_realistic_page_yields_its_article_and_drops_the_chrome() {
        let page = realistic_page();
        let article = extract(page.as_bytes(), "https://example.com/fox").unwrap();
        assert_eq!(article.title, "The Fox Report");
        assert_eq!(article.site.as_deref(), Some("Example Journal"));
        assert!(article.html.contains("quick brown fox"));
        assert!(!article.html.contains("Trending"), "sidebar survived: {}", article.html);
        assert!(!article.html.contains("Log in"), "nav survived: {}", article.html);
        assert!(!article.html.contains("newsletter"), "footer survived: {}", article.html);
    }

    #[test]
    fn relative_hrefs_come_out_absolute() {
        let page = realistic_page();
        let article = extract(page.as_bytes(), "https://example.com/fox").unwrap();
        assert!(article.html.contains(r#"href="https://example.com/methodology""#),
                "relative link not resolved: {}", article.html);
    }

    /// Every awkwardness here is deliberate: an unclosed `<p>` and `<div>`,
    /// misnested `<b><i></b></i>`, and unquoted attributes. Surviving this is
    /// html5ever's job -- the WHATWG error-recovery algorithm -- and the test
    /// only proves the pipeline hands the mess to it rather than falling over.
    #[test]
    fn broken_html_still_extracts() {
        let p = "Even a page whose author never closed a tag in their life \
                 deserves to be read, and the parser that browsers use treats \
                 that markup exactly the way the browsers of the world do, \
                 recovering a sensible tree from the wreckage every time.";
        let page = format!("<html><title>Wreckage</title><body>\
                            <div class=content>\
                            <p>{p}<p><b><i>{p}</b></i><p>{p}\
                            <div><p>{p}");
        let article = extract(page.as_bytes(), "https://example.com/wreck").unwrap();
        assert_eq!(article.title, "Wreckage");
        assert!(article.html.contains("wreckage every time"));
    }

    #[test]
    fn a_windows_1252_page_decodes_its_accents() {
        let page = realistic_page()
            .replace(r#"<meta charset="utf-8">"#,
                     r#"<meta http-equiv=Content-Type content="text/html; charset=windows-1252">"#)
            .replace("The quick brown fox", "The caf\u{e9} fox");
        // Encode the fixture as real windows-1252 bytes: the é must travel as
        // the single byte 0xE9, which read as UTF-8 would be a decode error.
        let (bytes, _, had_unmappable) = encoding_rs::WINDOWS_1252.encode(&page);
        assert!(!had_unmappable);
        assert!(bytes.contains(&0xE9));
        let article = extract(&bytes, "https://example.com/cafe").unwrap();
        assert!(article.html.contains("caf\u{e9} fox"),
                "é lost in decoding: {}", article.html);
    }

    #[test]
    fn a_byte_order_mark_beats_the_meta_declaration() {
        // A UTF-8 page whose meta *lies* about being windows-1252 -- with a
        // BOM, which per the Encoding Standard outranks any declaration.
        let page = realistic_page()
            .replace(r#"<meta charset="utf-8">"#, r#"<meta charset="windows-1252">"#)
            .replace("The quick brown fox", "The caf\u{e9} fox");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(page.as_bytes());
        let article = extract(&bytes, "https://example.com/bom").unwrap();
        assert!(article.html.contains("caf\u{e9} fox"));
    }

    /// The saltfatacidheat.com ragù page, reduced to its skeleton: an
    /// ingredient list of short lines in one text block, and the long-prose
    /// instructions in another -- but the instructions sit one level deeper,
    /// sharing a grid row with an image column. Without the grid flattening,
    /// readability picks the instructions as its top candidate and looks for
    /// the rest of the article among their *siblings*; the ingredient block,
    /// an aunt in the grid, never gets visited, and the recipe comes out with
    /// no ingredients (verified against the live page, 2026-08-16).
    #[test]
    fn a_grid_layout_does_not_hide_part_of_the_article() {
        let step = "Set a large pot over high heat and add enough olive oil \
                    to coat the bottom, then crumble the beef into the pot in \
                    walnut-size pieces, stirring and breaking up the meat \
                    until it sizzles, browns and smells like dinner.";
        let ingredients: String = [
            "Approximately 1 cup (200 grams) extra-virgin olive oil",
            "1 pound (450 grams) coarsely ground beef chuck",
            "1 pound (450 grams) coarsely ground pork shoulder",
            "2 medium yellow onions, minced", "1 large carrot, minced",
            "2 large celery stalks, minced", "2 cups (450 grams) whole milk",
            "2 bay leaves", "5 tablespoons (80 grams) tomato paste",
            "Parmesan rind", "Salt", "Freshly ground black pepper",
        ].map(|line| format!("<p>{line}</p>")).concat();
        let page = format!(r#"<!DOCTYPE html>
<html><head><title>Benedetta's Ragú - Example Kitchen</title></head>
<body>
<nav><a href="/">Home</a> <a href="/recipes">Recipes</a></nav>
<div class="layout grid-12 columns-12">
  <div class="row">
    <div class="col-12">
      <div class="block html-block">
        <div class="block-content"><div class="html-content">{ingredients}</div></div>
      </div>
      <div class="row">
        <div class="col-8">
          <div class="block html-block">
            <div class="block-content"><div class="html-content">
              <p>{step}</p><p>{step}</p><p>{step}</p><p>{step}</p><p>{step}</p>
            </div></div>
          </div>
        </div>
        <div class="col-4"><div class="block image-block"></div></div>
      </div>
    </div>
  </div>
</div>
<footer>Copyright 2026. Subscribe to the newsletter.</footer>
</body></html>"#);
        let article = extract(page.as_bytes(), "https://example.com/ragu").unwrap();
        assert!(article.html.contains("walnut-size pieces"),
                "instructions lost: {}", article.html);
        assert!(article.html.contains("extra-virgin olive oil"),
                "ingredients lost: {}", article.html);
        assert!(article.html.contains("Freshly ground black pepper"),
                "ingredient tail lost: {}", article.html);
        assert!(!article.html.contains("newsletter"), "footer survived: {}", article.html);
    }

    #[test]
    fn a_page_with_no_article_is_an_err() {
        let page = r#"<!DOCTYPE html>
<html><head><title>Sign in</title></head>
<body>
<nav><a href="/home">Home</a></nav>
<form action="/session" method="post">
<label>Email <input type="email" name="email"></label>
<label>Password <input type="password" name="password"></label>
<button>Sign in</button>
</form>
<a href="/forgot">Forgot password?</a> <a href="/register">Create account</a>
</body></html>"#;
        let err = extract(page.as_bytes(), "https://example.com/login").unwrap_err();
        assert!(format!("{err:#}").contains("no readable article found"));
    }
}
