//! One article, extracted and re-set -- the deliberate reversal of this
//! module's founding scope line.
//!
//! The manifesto in `news/mod.rs` said "no readability heuristics", and for
//! *sources* it still holds: a front page or a feed has a structured form, and
//! structured data is read as structured data. But the story a headline links
//! to has no such form, and queueing it for another machine was this reader
//! declining to read. So the position is now: structured data where it exists,
//! Readability where it doesn't. This source takes the article's own URL as
//! its route, hands the fetched bytes to the injected [`ArticleExtractor`],
//! and treats what comes back exactly like every other stranger's fragment --
//! through the sanitizer, which stays the unbreached boundary between the
//! web's HTML and the layout engine. (An article gets the sanitizer's one
//! wider vocabulary, `sanitize_article_fragment`: its images survive, because
//! opening the article was the reader's own choice in a way that a comment's
//! tracking pixel never is.) The heuristics themselves live behind
//! the trait, out of core; what this file adds is only the page around their
//! answer.

use anyhow::{format_err, Error};
use fxhash::FxHashMap;
use std::fmt::Write;
use std::sync::Arc;

use crate::helpers::decode_entities;
use super::{escape_text, host_of, sanitize_article_fragment};
use super::{ArticleExtractor, HttpClient, Page, Route, Source};

pub const ID: &str = "article";

/// The source behind every "open this link here" tap. It never appears in the
/// source menu -- it has no front page to offer -- and its `Route::Thread` is
/// the article URL itself rather than an id, because an article's URL *is* its
/// identity.
pub struct ArticleSource {
    extractor: Arc<dyn ArticleExtractor>,
}

impl ArticleSource {
    /// Cheap on purpose: the view builds one of these per worker thread, and
    /// unlike a feed this source is machinery rather than three strings -- so
    /// the machinery arrives behind an `Arc` and every copy shares it.
    pub fn new(extractor: Arc<dyn ArticleExtractor>) -> ArticleSource {
        ArticleSource { extractor }
    }
}

impl Source for ArticleSource {
    fn id(&self) -> &str {
        ID
    }

    fn title(&self) -> &str {
        "Article"
    }

    fn url(&self, route: &Route) -> Result<String, Error> {
        match route {
            Route::Index => Err(format_err!("an article source has no front page")),
            Route::Thread(url) => {
                // The string reaches the HTTP client, so it is checked rather
                // than trusted -- the same reasoning as hn.rs's digit check.
                if url.starts_with("http://") || url.starts_with("https://") {
                    Ok(url.clone())
                } else {
                    Err(format_err!("not a web URL: {url:?}"))
                }
            }
        }
    }

    fn render(&self, route: &Route, raw: &[u8], _now: i64) -> Result<Page, Error> {
        let Route::Thread(url) = route else {
            return Err(format_err!("an article source has no front page"));
        };
        let article = self.extractor.extract(raw, url)?;

        // A page with no findable title still needs a title bar; the host is
        // the most honest short name for it.
        let title = match article.title.trim() {
            "" => host_of(url).to_string(),
            found => found.to_string(),
        };

        let mut body = String::with_capacity(article.html.len() + 512);
        body.push_str("<div class=\"head\">");
        // Plain text, no link: the head of an article is the one place where
        // tapping its title could only reopen the page it is on.
        let _ = write!(body, "<h1>{}</h1>", escape_text(&title));

        let mut meta = Vec::new();
        if let Some(byline) = article.byline.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            meta.push(escape_text(byline));
        }
        let site = article.site.as_deref().map(str::trim).filter(|s| !s.is_empty())
                          .unwrap_or_else(|| host_of(url));
        meta.push(escape_text(site));
        let _ = write!(body, "<div class=\"meta\">{}</div>", meta.join(" · "));
        body.push_str("</div>");

        let _ = write!(body, "<div class=\"article\">{}</div>",
                       sanitize_article_fragment(&article.html));

        Ok(Page::text(title, body))
    }

    /// Overridden for the same reason hn.rs overrides it -- one route, more
    /// than one request -- while `render` itself stays a pure function of the
    /// page's bytes. The extra requests are the article's images: `render`
    /// leaves them as sanitized `<img src="https://..."/>` references, and
    /// this fetches each one through the same injected client and rewrites
    /// the srcs to in-memory names that travel with the [`Page`].
    fn load(&self, route: &Route, http: &dyn HttpClient, now: i64) -> Result<Page, Error> {
        let raw = http.get(&self.url(route)?)?;
        let mut page = self.render(route, &raw, now)?;
        page.images = fetch_images(&mut page.body, http);
        Ok(page)
    }
}

/// At most this many images are fetched per article; the rest keep a name
/// that maps to nothing, which the engine draws as nothing. A dozen figures
/// is already a generous article, and each one is a round trip on a radio
/// that is the slowest part of this whole feature.
const MAX_IMAGES: usize = 12;

/// ... and at most this many bytes of image data, checked before each fetch,
/// so one photo essay cannot fill the device's memory: the map is cloned into
/// the view's history, and 8 MB is already a lot to hold twice.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Rewrite the body's image srcs to `img-0`, `img-1`, ... and return the
/// fetched bytes under those names.
///
/// The body is our own sanitizer's output, so every image in it is exactly
/// `<img src="..."/>` with a double-quoted, entity-escaped web URL -- a plain
/// scan is enough, and no other `<img` can occur because a literal one in the
/// article's text arrives as `&lt;img`. Kept separate from [`Source::load`]
/// so a test can drive it with a fake client and no network.
///
/// A fetch that fails, or one skipped by the caps above, still leaves its tag
/// pointing at a name with no resource behind it: the engine already treats
/// an unfetchable image as an image that is not there, which is the graceful
/// end this reader wants for a broken figure. The same URL appearing twice is
/// fetched once and shares a name.
fn fetch_images(body: &mut String, http: &dyn HttpClient) -> FxHashMap<String, Vec<u8>> {
    const PREFIX: &str = "<img src=\"";

    let mut images = FxHashMap::default();
    let mut names: FxHashMap<String, String> = FxHashMap::default();
    let mut total = 0;
    let mut out = String::with_capacity(body.len());
    let mut rest = body.as_str();

    while let Some(at) = rest.find(PREFIX) {
        let start = at + PREFIX.len();
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // The closing quote is always there -- see the shape argument above --
        // but a scanner that assumed it would still be wrong to write.
        let Some(end) = rest.find('"') else { break };
        let src = &rest[..end];
        rest = &rest[end..];

        let name = match names.get(src) {
            Some(name) => name.clone(),
            None => {
                let name = format!("img-{}", names.len());
                names.insert(src.to_string(), name.clone());
                if names.len() <= MAX_IMAGES && total < MAX_IMAGE_BYTES {
                    // The src went through HTML twice (extractor, sanitizer),
                    // so its `&amp;` must become `&` again before it can name
                    // a resource on a wire.
                    if let Ok(bytes) = http.get(&decode_entities(src)) {
                        total += bytes.len();
                        images.insert(name.clone(), bytes);
                    }
                }
                name
            }
        };
        out.push_str(&name);
    }

    out.push_str(rest);
    *body = out;
    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::html::xml::XmlParser;
    use crate::news::ExtractedArticle;

    /// The canned answer a test injects where `plato-article` would be.
    struct Canned(ExtractedArticle);

    impl ArticleExtractor for Canned {
        fn extract(&self, _raw: &[u8], _url: &str) -> Result<ExtractedArticle, Error> {
            Ok(self.0.clone())
        }
    }

    fn source(article: ExtractedArticle) -> ArticleSource {
        ArticleSource::new(Arc::new(Canned(article)))
    }

    fn extracted() -> ExtractedArticle {
        ExtractedArticle {
            title: "A Modest Proposal".to_string(),
            byline: Some("Jonathan Swift".to_string()),
            site: Some("The Examiner".to_string()),
            html: "<p>It is a melancholy object.</p>".to_string(),
        }
    }

    #[test]
    fn only_web_urls_reach_the_http_client() {
        let source = source(extracted());
        assert_eq!(source.url(&Route::Thread("https://example.com/a".into())).unwrap(),
                   "https://example.com/a");
        assert_eq!(source.url(&Route::Thread("http://example.com/a".into())).unwrap(),
                   "http://example.com/a");
        assert!(source.url(&Route::Thread("file:///etc/passwd".into())).is_err());
        assert!(source.url(&Route::Thread("javascript:alert(1)".into())).is_err());
        // There is no front page to ask for.
        assert!(source.url(&Route::Index).is_err());
    }

    #[test]
    fn an_article_renders_head_then_body() {
        let page = source(extracted())
            .render(&Route::Thread("https://www.example.com/essay".into()), b"", 0)
            .unwrap();
        assert_eq!(page.title, "A Modest Proposal");
        assert!(page.body.contains("<h1>A Modest Proposal</h1>"));
        assert!(page.body.contains("<div class=\"meta\">Jonathan Swift · The Examiner</div>"));
        assert!(page.body.contains("<div class=\"article\"><p>It is a melancholy object.</p></div>"));
        // The head's title is text, not a link to the page it is on.
        assert!(!page.body.contains("<h1><a"));
    }

    #[test]
    fn a_nameless_page_borrows_its_host() {
        let mut article = extracted();
        article.title = "  ".to_string();
        article.byline = None;
        article.site = None;
        let page = source(article)
            .render(&Route::Thread("https://www.example.com/essay".into()), b"", 0)
            .unwrap();
        assert_eq!(page.title, "example.com");
        assert!(page.body.contains("<div class=\"meta\">example.com</div>"));
    }

    /// The property the whole design leans on: the extractor's output is a
    /// stranger's HTML, and none of its weapons survive the sanitizer. An
    /// `img` is no longer one of those weapons *here* -- an article's images
    /// are part of what the reader asked to open -- but it survives stripped
    /// to its src, and the src never reaches the network except through the
    /// injected client in `load`.
    #[test]
    fn hostile_extractor_output_cannot_reach_the_page_unsanitized() {
        let mut article = extracted();
        article.html = "<p onclick=\"boom()\">fine</p>\
                        <script>alert(1)</script>\
                        <img src=\"http://tracker/x.gif\" onerror=\"boom()\">\
                        <figure><figcaption>Photo: nobody</figcaption></figure>\
                        <h2>heading<p>unclosed"
            .to_string();
        let page = source(article)
            .render(&Route::Thread("https://example.com/a".into()), b"", 0)
            .unwrap();
        assert!(!page.body.contains("script"));
        assert!(!page.body.contains("alert"));
        assert!(!page.body.contains("onclick"));
        assert!(!page.body.contains("onerror"));
        assert!(page.body.contains("<img src=\"http://tracker/x.gif\"/>"));
        // The figure unwraps -- its image is drawn now, so its caption is
        // information, kept as a styleable element rather than dropped.
        assert!(page.body.contains("<figcaption>Photo: nobody</figcaption>"));
        // The unclosed pair is balanced before it can leak out of the body.
        assert!(page.body.contains("<h2>heading<p>unclosed</p></h2></div>"));
    }

    /// A 1x1 grayscale PNG -- real enough for MuPDF, three lines with the
    /// `png` crate that is already a dependency.
    fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 1, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0]).unwrap();
        drop(writer);
        out
    }

    /// The whole wire, minus the wire: a fake extractor emits imgs, a fake
    /// client serves exactly one of them, and the page that comes out of
    /// `load` is what the view would show.
    struct ServesOnePicture;

    impl HttpClient for ServesOnePicture {
        fn get(&self, url: &str) -> Result<Vec<u8>, Error> {
            match url {
                "https://example.com/essay" => Ok(Vec::new()),
                "https://example.com/pic.png?a=1&b=2" => Ok(tiny_png()),
                _ => Err(format_err!("404: {url}")),
            }
        }
    }

    #[test]
    fn an_articles_images_are_fetched_renamed_and_carried_with_the_page() {
        let mut article = extracted();
        // One fetchable image (its URL entity-escaped, as HTML attributes
        // are), one that will 404, one `data:` URI, and the first one again.
        article.html = "<p>one</p><img src=\"https://example.com/pic.png?a=1&amp;b=2\">\
                        <p>two</p><img src=\"https://example.com/gone.png\">\
                        <img src=\"data:image/gif;base64,R0lGOD\">\
                        <img src=\"https://example.com/pic.png?a=1&amp;b=2\">"
            .to_string();
        let page = source(article)
            .load(&Route::Thread("https://example.com/essay".into()), &ServesOnePicture, 0)
            .unwrap();

        // Every surviving src is now an in-memory name; no URL is left for
        // the layout engine to see, and the data: URI died in the sanitizer.
        assert!(page.body.contains("<img src=\"img-0\"/><p>two</p><img src=\"img-1\"/><img src=\"img-0\"/>"));
        assert!(!page.body.contains("https://example.com/pic.png"));
        assert!(!page.body.contains("data:"));

        // Only the served image has bytes; img-1 names nothing, which the
        // engine draws as nothing.
        assert_eq!(page.images.len(), 1);
        assert_eq!(page.images.get("img-0"), Some(&tiny_png()));

        // And the page is still a well-formed fragment.
        let doc = format!("<body>{}</body>", page.body);
        let tree = XmlParser::new(&doc).parse();
        assert!(tree.root().text().contains("two"));
    }

    #[test]
    fn a_rendered_article_parses_as_xml() {
        let mut article = extracted();
        article.title = "Q&A: a < b".to_string();
        article.html = "<p>see <a href='u'>x<i>y</b></a><div><hr>end".to_string();
        let page = source(article)
            .render(&Route::Thread("https://example.com/a".into()), b"", 0)
            .unwrap();
        let doc = format!("<body>{}</body>", page.body);
        let tree = XmlParser::new(&doc).parse();
        assert!(tree.root().text().contains("end"));
    }
}
