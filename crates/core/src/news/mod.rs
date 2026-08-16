//! Superlight reading of a few known sites -- and of the articles they point
//! at.
//!
//! This is not a browser and is not a step towards one. Every listing source
//! here returns *structured* data -- Hacker News as JSON, feeds as XML --
//! which this module turns into markup **we** wrote, for `document::html` to
//! render: no tables, no floats, no JavaScript, and no surprises when a site
//! redesigns. The original scope line went further -- "no readability
//! heuristics" -- and that line has been deliberately crossed, once, by
//! [`article`]: the story a headline links to has no structured form, so it
//! goes through an injected readability engine and comes back as untrusted
//! HTML that `sanitize_fragment` reduces to the same small vocabulary as
//! everything else. Structured data where it exists, Readability where it
//! doesn't, and the sanitizer as the unbreached boundary either way.
//!
//! Adding a site means implementing [`Source`]: say which URL a route needs,
//! and turn the bytes into a [`Page`]. Fetching is a separate trait so that
//! every renderer is a pure function of (bytes, clock) and can be tested
//! without a network.

pub mod article;
pub mod feed;
pub mod hn;
mod sanitize;

pub use sanitize::{escape_attribute, escape_text, sanitize_fragment, text_only};

use anyhow::Error;

/// The one thing `plato-core` needs from the network, kept as a trait so core
/// stays network-free: rustls, roots and timeouts live in `plato-net`, which
/// the `plato` binary injects at startup.
pub trait HttpClient: Send + Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, Error>;
}

/// What `plato-core` needs from a readability engine, as a trait for the same
/// reason [`HttpClient`] is one: the engine (html5ever and the Readability
/// scoring, in `plato-article`) stays out of core, and the front ends inject
/// it at startup. What comes back is *untrusted* markup -- the extractor's
/// idea of the article, still written in the site's own HTML -- and it goes
/// through `sanitize_fragment` like every other stranger's fragment before it
/// can reach the layout engine.
pub trait ArticleExtractor: Send + Sync {
    fn extract(&self, raw: &[u8], url: &str) -> Result<ExtractedArticle, Error>;
}

/// A readability engine's answer: metadata worth a head block, and the article
/// body as HTML that has been *found* but not yet made trustworthy.
#[derive(Debug, Clone)]
pub struct ExtractedArticle {
    pub title: String,
    pub byline: Option<String>,
    pub site: Option<String>,
    pub html: String,
}

/// Where you are within a source. Two shapes cover everything this reader is
/// for: a list of things, and one thing with its discussion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Index,
    Thread(String),
}

/// A rendered page: a fragment of XHTML for `HtmlDocument::new_from_memory`,
/// plus what to put in the title bar. Cloneable because it travels from the
/// worker thread to the event loop inside an `Event`.
#[derive(Debug, Clone)]
pub struct Page {
    pub title: String,
    pub body: String,
}

pub trait Source: Send + Sync {
    /// Stable, short, and part of the link scheme -- so it lands in saved
    /// state and must not change casually.
    fn id(&self) -> &str;
    fn title(&self) -> &str;
    fn url(&self, route: &Route) -> Result<String, Error>;
    /// `now` is Unix seconds, passed in rather than read, so that a rendering
    /// test pins the "3 hours ago" text instead of racing it.
    fn render(&self, route: &Route, raw: &[u8], now: i64) -> Result<Page, Error>;

    /// Fetch and render. Overridden only by a source that needs more than one
    /// request for a route -- Hacker News does, for its front page -- so that
    /// the common case stays a pure `render` over bytes a test can supply.
    fn load(&self, route: &Route, http: &dyn HttpClient, now: i64) -> Result<Page, Error> {
        let raw = http.get(&self.url(route)?)?;
        self.render(route, &raw, now)
    }
}

pub fn load(source: &dyn Source, route: &Route, http: &dyn HttpClient, now: i64)
            -> Result<Page, Error> {
    source.load(route, http, now)
}

/// Links the view intercepts, distinguishable from an `https:` link -- which
/// the view now opens in-reader through the article source.
pub fn route_uri(source_id: &str, route: &Route) -> String {
    match route {
        Route::Index => format!("news:{source_id}/index"),
        Route::Thread(id) => format!("news:{source_id}/thread/{id}"),
    }
}

pub fn parse_route_uri(uri: &str) -> Option<(&str, Route)> {
    let rest = uri.strip_prefix("news:")?;
    let (id, rest) = rest.split_once('/')?;
    match rest {
        "index" => Some((id, Route::Index)),
        _ => rest.strip_prefix("thread/")
                 .map(|thread| (id, Route::Thread(thread.to_string()))),
    }
}

/// "3h", "2d" -- the compact form, because it sits in a line of metadata that
/// is already long and is read at a glance.
pub fn relative_time(then: i64, now: i64) -> String {
    let secs = (now - then).max(0);
    match secs {
        s if s < 60 => "now".to_string(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 30 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}mo", s / (30 * 86_400)),
    }
}

/// The bit of a URL worth showing next to a headline. Cheap string work rather
/// than a URL parser: this is display text, and a wrong answer costs a
/// misleading label, not a bad request.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_uris_round_trip() {
        for route in [Route::Index, Route::Thread("49299605".to_string())] {
            let uri = route_uri("hn", &route);
            assert_eq!(parse_route_uri(&uri), Some(("hn", route)));
        }
        assert_eq!(parse_route_uri("https://example.com"), None);
    }

    #[test]
    fn relative_times() {
        let now = 1_786_800_092;
        assert_eq!(relative_time(now - 30, now), "now");
        assert_eq!(relative_time(now - 600, now), "10m");
        assert_eq!(relative_time(now - 3 * 3_600, now), "3h");
        assert_eq!(relative_time(now - 5 * 86_400, now), "5d");
        // A clock that has not been synced yet must not print a negative age.
        assert_eq!(relative_time(now + 900, now), "now");
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://www.pcworld.com/article/3212428/x.html"), "pcworld.com");
        assert_eq!(host_of("https://simonwillison.net/atom/everything/"), "simonwillison.net");
        // A relative URL has no host; the first path segment is a poor label
        // but a harmless one, and nothing this reader generates gets here.
        assert_eq!(host_of("item?id=49299605"), "item");
    }
}
