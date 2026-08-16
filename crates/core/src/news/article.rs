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
//! through `sanitize_fragment`, which stays the unbreached boundary between
//! the web's HTML and the layout engine. The heuristics themselves live behind
//! the trait, out of core; what this file adds is only the page around their
//! answer.

use anyhow::{format_err, Error};
use std::fmt::Write;
use std::sync::Arc;

use super::{escape_text, host_of, sanitize_fragment};
use super::{ArticleExtractor, Page, Route, Source};

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
                       sanitize_fragment(&article.html));

        Ok(Page { title, body })
    }
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
    /// stranger's HTML, and none of its weapons survive the sanitizer.
    #[test]
    fn hostile_extractor_output_cannot_reach_the_page_unsanitized() {
        let mut article = extracted();
        article.html = "<p onclick=\"boom()\">fine</p>\
                        <script>alert(1)</script>\
                        <img src=\"http://tracker/x.gif\">\
                        <figure><figcaption>Photo: nobody</figcaption></figure>\
                        <h2>heading<p>unclosed"
            .to_string();
        let page = source(article)
            .render(&Route::Thread("https://example.com/a".into()), b"", 0)
            .unwrap();
        assert!(!page.body.contains("script"));
        assert!(!page.body.contains("alert"));
        assert!(!page.body.contains("onclick"));
        assert!(!page.body.contains("img"));
        assert!(!page.body.contains("Photo"));
        // The unclosed pair is balanced before it can leak out of the body.
        assert!(page.body.contains("<h2>heading<p>unclosed</p></h2></div>"));
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
