//! The `news` renderers against payloads a real server actually sent.
//!
//! The unit tests in `news/*` run on hand-written fixtures, which pin the
//! awkward cases but say nothing about whether the mapping matches what
//! Algolia and these feeds emit today. This closes that gap, and it is also
//! how a feed that changes shape gets noticed: capture again, run this.
//!
//! It needs captures, and none are committed -- they are other people's words
//! and hundreds of kilobytes each. So it is skipped unless a directory is
//! named, in the same style as `pdf_layout.rs`:
//!
//! ```text
//! mkdir -p /tmp/news && cd /tmp/news
//! curl -so hn-order.json  https://hacker-news.firebaseio.com/v0/topstories.json
//! ids=$(python3 -c "import json;print(','.join('story_%d'%i for i in json.load(open('hn-order.json'))[:30]))")
//! curl -so hn-index.json -G https://hn.algolia.com/api/v1/search \
//!      --data-urlencode "tags=($ids),story" --data-urlencode hitsPerPage=30
//! curl -so hn-thread.json 'https://hn.algolia.com/api/v1/items/49299605'
//! curl -sLo simonwillison.xml https://simonwillison.net/atom/everything/
//! curl -sLo theverge.xml      https://www.theverge.com/rss/index.xml
//! curl -sLo electrek.xml      https://electrek.co/feed/
//!
//! PLATO_TEST_NEWS_DIR=/tmp/news python3 xbuild.py host --test --package plato-core
//! ```
//!
//! Run it with `-- --nocapture` to read the rendered text, which is the
//! fastest way to judge a change to the markup without starting the emulator.

use std::env;
use std::fs;
use std::path::PathBuf;

use plato_core::anyhow::{format_err, Error};
use plato_core::document::html::xml::XmlParser;
use plato_core::news::feed::Feed;
use plato_core::news::hn::HackerNews;
use plato_core::news::{HttpClient, Route, Source};

/// Replays the captures in place of the network, so the *two-request* front
/// page is exercised the way the device runs it rather than through `render`
/// alone -- the ordering and the id-set filtering only exist on that path.
struct Captured(PathBuf);

impl HttpClient for Captured {
    fn get(&self, url: &str) -> Result<Vec<u8>, Error> {
        let name = if url.contains("topstories") {
            "hn-order.json"
        } else if url.contains("/items/") {
            "hn-thread.json"
        } else {
            "hn-index.json"
        };
        fs::read(self.0.join(name)).map_err(|e| format_err!("{name}: {e}"))
    }
}

fn captures() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("PLATO_TEST_NEWS_DIR")?);
    path.is_dir().then_some(path)
}

/// A rendering is only worth anything if `document::html` can parse it, so
/// every case ends here: parse the fragment, and return its text.
fn parsed_text(body: &str) -> String {
    let doc = format!("<body>{body}</body>");
    XmlParser::new(&doc).parse().root().text()
}

const NOW: i64 = 1_786_800_092;

/// The Paperwhite 3's panel, minus the two bars the news view keeps: what the
/// page is actually laid out into, at the settings' defaults.
const PAGE: (u32, u32) = (1072, 1338);
const FONT_SIZE: f32 = 11.0;
const MARGIN_WIDTH: i32 = 4;

fn news_document(body: &str) -> plato_core::document::html::HtmlDocument {
    use plato_core::document::Document;

    // The engine reads `fonts/` and `css/` relative to the working directory,
    // which for a test is the crate, not the repository.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    env::set_current_dir(&root).ok();

    let mut doc = plato_core::document::html::HtmlDocument::new_from_memory(body);
    doc.set_viewer_stylesheet("css/news.css");
    doc.layout(PAGE.0, PAGE.1, FONT_SIZE, 300);
    doc.set_margin_width(MARGIN_WIDTH);
    doc
}

/// Write the first pages as PNGs, at the exact size the panel will draw them.
///
/// The stylesheet is the design, and a stylesheet cannot be reviewed by reading
/// it: indentation that looks generous in CSS is a four-word column at 1072 px,
/// and grey that reads fine on a monitor may vanish on e-ink. So this renders
/// through the same engine the device uses and leaves the sheets in `$TMPDIR`
/// to be looked at.
fn dump_pages(body: &str, name: &str, count: usize) {
    use plato_core::document::{Document, Location};
    // `save` is a Framebuffer method, and a Pixmap is one.
    use plato_core::framebuffer::Framebuffer;

    let mut doc = news_document(body);
    let mut loc = Location::Exact(0);
    for page in 0..count {
        let Some((pixmap, next)) = doc.pixmap(loc, 1.0, 1) else { break };
        let out = env::temp_dir().join(format!("news-{name}-{page}.png"));
        pixmap.save(out.to_str().unwrap()).unwrap();
        println!("{}", out.display());
        loc = Location::Next(next);
    }
}

#[test]
fn the_front_page_renders() {
    let Some(dir) = captures() else { return };
    let http = Captured(dir.clone());

    let page = HackerNews.load(&Route::Index, &http, NOW).unwrap();
    let text = parsed_text(&page.body);

    // Up to 30 stories, each with a discussion link and a metadata line. Not
    // exactly 30: YC ranks job posts on the front page and Algolia does not
    // index them as stories, so one or two of the ids routinely have no hit
    // and are skipped. What must hold is that nothing else is lost -- a
    // capture that renders far fewer means the query is wrong, which is
    // exactly what this test caught when the tag filter let comments through.
    let stories = page.body.matches("class=\"story\"").count();
    assert!((27..=30).contains(&stories), "{stories} stories rendered, expected 27..=30");
    assert_eq!(page.body.matches("href=\"news:hn/thread/").count(), 2 * stories);
    assert!(text.contains(" points · "));

    // Hacker News's own ranking, not Algolia's relevance order: the ids
    // rendered must be a subsequence of topstories.json -- same order, with
    // the skipped jobs as the only gaps. A prefix check would be the stronger
    // statement but it is not true: the top-ranked item can be the job post.
    let order: Vec<String> = plato_core::serde_json::from_slice::<Vec<i64>>(
                                 &http.get("topstories").unwrap()).unwrap()
                                 .iter().map(|id| id.to_string()).collect();
    let rendered: Vec<String> = page.body.split("<div class=\"story\">").skip(1)
                                    .filter_map(|story| {
                                        story.split_once("<a href=\"news:hn/thread/")
                                             .and_then(|(_, rest)| rest.split_once('"'))
                                             .map(|(id, _)| id.to_string())
                                    })
                                    .collect();
    assert_eq!(rendered.len(), stories, "a story rendered without a discussion link");
    let mut ranking = order.iter();
    for id in &rendered {
        assert!(ranking.any(|ranked| ranked == id),
                "{id} is out of Hacker News's ranking, or not in it at all");
    }
    println!("--- front page ({} bytes rendered) ---\n{}", page.body.len(), text);
    dump_pages(&page.body, "front", 2);
}

#[test]
fn a_large_thread_renders() {
    let Some(dir) = captures() else { return };
    let raw = fs::read(dir.join("hn-thread.json")).expect("hn-thread.json");

    let page = HackerNews.render(&Route::Thread("49299605".into()), &raw, NOW).unwrap();
    let text = parsed_text(&page.body);

    let comments = page.body.matches("class=\"comment d").count();
    assert!(comments > 500, "only {comments} comments rendered");
    // The point of the JSON route: the markup we generate is a fraction of the
    // 1,094,659 bytes the same thread costs as HTML.
    assert!(page.body.len() < raw.len(), "rendered {} from {} raw", page.body.len(), raw.len());
    assert!(!text.is_empty());
    println!("--- thread: {} comments, {} bytes raw -> {} bytes rendered ---",
             comments, raw.len(), page.body.len());
    println!("{}", &text.chars().take(1_500).collect::<String>());

    dump_pages(&page.body, "thread", 3);
    paginate(&page.body, "thread");
}

/// The end of the pipeline: hand the generated markup to the same engine the
/// reader uses, at the device's geometry, and see how many pages it makes and
/// how long it takes. On the Mac this is a sanity check; the number worth
/// watching is the *time*, because the device's Cortex-A9 will want roughly an
/// order of magnitude more of it, and the biggest thread on Hacker News is the
/// worst case this reader will ever meet.
fn paginate(body: &str, what: &str) {
    use std::time::Instant;
    use plato_core::document::{Document, Location};

    let mut doc = news_document(body);

    // `pages_count` on an HtmlDocument is a byte count -- pagination is lazy,
    // so the only way to time it is to walk it, which is also what the reader
    // does when you turn pages to the end.
    let start = Instant::now();
    let mut pages = 0;
    let mut loc = Location::Exact(0);
    while let Some((_, next)) = doc.links(loc) {
        pages += 1;
        loc = Location::Next(next);
        if pages > 10_000 {
            break;
        }
    }
    println!("--- {what}: {} bytes -> {} pages, walked in {} ms ---",
             body.len(), pages, start.elapsed().as_millis());
    assert!(pages > 0);
}

#[test]
fn the_configured_feeds_render() {
    let Some(dir) = captures() else { return };

    for (file, id, title) in [("simonwillison.xml", "simonw", "Simon Willison"),
                              ("theverge.xml", "verge", "The Verge"),
                              ("electrek.xml", "electrek", "Electrek")] {
        let path = dir.join(file);
        if !path.is_file() {
            continue;
        }
        let raw = fs::read(&path).unwrap();
        let feed = Feed::new(id, title, "https://example.test/feed");
        let page = feed.render(&Route::Index, &raw, NOW).unwrap();
        let text = parsed_text(&page.body);

        let entries = page.body.matches("class=\"story\"").count();
        assert!(entries >= 5, "{file}: only {entries} entries");
        assert!(!text.trim().is_empty(), "{file}: rendered no text");
        // Nothing may reach the screen still escaped -- the failure mode of
        // decoding a feed's HTML one time too few.
        assert!(!text.contains("&lt;p&gt;"), "{file}: blurb left escaped");

        println!("--- {file}: {entries} entries, {} bytes rendered ---", page.body.len());
        println!("{}", &text.chars().take(800).collect::<String>());
        dump_pages(&page.body, id, 1);
    }
}
