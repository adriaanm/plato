//! Hacker News, through Algolia's public API.
//!
//! Why the API and not the site: measured on one 724-comment thread, the HTML
//! page is 1,094,659 bytes of nested tables whose `<p>` never closes and whose
//! comment indentation is carried by `<img src="s.gif" width="40">` spacers;
//! the same thread as JSON is 384,043 bytes with the nesting *in the data*.
//! One request either way. So the JSON costs a third of the bytes, a third of
//! the parsing, and none of the table layout `document::html` does not have.
//!
//! The API needs no key and no account. Its one weakness is that it is a
//! third party (the search index, not YC), so `url()` and `render()` are kept
//! apart: pointing this at a different endpoint, or at scraped HTML, is a new
//! `Source`, not a rewrite.

use anyhow::{format_err, Error};
use fxhash::FxHashMap;
use serde::Deserialize;
use std::fmt::Write;

use super::{escape_attribute, escape_text, host_of, relative_time, route_uri, sanitize_fragment};
use super::{HttpClient, Page, Route, Source};

const ID: &str = "hn";

/// The front page takes two requests, and it is worth knowing why.
///
/// Algolia's own `tags=front_page` looked like the one-request answer and is
/// not: it is a *historical* index -- 183 hits when this was written, sorted by
/// points, so a four-month-old story with 9 points sat in the same list as
/// today's top item. What YC itself publishes, `topstories.json`, is the real
/// ranked front page but gives only ids, and the v0 API has no batch call, so
/// reading it story by story would be 30 more requests.
///
/// So: ids from YC, then all 30 stories from Algolia in one query. Two
/// requests, ~28 KB, and the order is Hacker News's own.
const TOP_STORIES: &str = "https://hacker-news.firebaseio.com/v0/topstories.json";
const FRONT_PAGE_COUNT: usize = 30;

/// How far in the indentation is allowed to go. At 1072 px, past about eight
/// steps a reply is a column of one word per line; deeper comments keep the
/// last indent and stay readable, at the cost of ambiguity about who replied
/// to whom -- which is the better trade on a screen this narrow.
const MAX_DEPTH: usize = 7;

pub struct HackerNews;

impl Source for HackerNews {
    fn id(&self) -> &str {
        ID
    }

    fn title(&self) -> &str {
        "Hacker News"
    }

    fn url(&self, route: &Route) -> Result<String, Error> {
        Ok(match route {
            // The first of the two; `load` makes the second, which depends on
            // this one's answer.
            Route::Index => TOP_STORIES.to_string(),
            Route::Thread(id) => {
                if !id.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format_err!("not a Hacker News item id: {id:?}"));
                }
                format!("https://hn.algolia.com/api/v1/items/{id}")
            }
        })
    }

    fn render(&self, route: &Route, raw: &[u8], now: i64) -> Result<Page, Error> {
        match route {
            // No ranking to impose here, so the search's own order stands.
            // `load` is what supplies Hacker News's order.
            Route::Index => render_index(serde_json::from_slice(raw)?, &[], now),
            Route::Thread(..) => render_thread(serde_json::from_slice(raw)?, now),
        }
    }

    fn load(&self, route: &Route, http: &dyn HttpClient, now: i64) -> Result<Page, Error> {
        if !matches!(route, Route::Index) {
            let raw = http.get(&self.url(route)?)?;
            return self.render(route, &raw, now);
        }

        let ids: Vec<i64> = serde_json::from_slice(&http.get(TOP_STORIES)?)?;
        let ids: Vec<String> = ids.iter().take(FRONT_PAGE_COUNT)
                                  .map(|id| id.to_string()).collect();
        let raw = http.get(&batch_url(&ids))?;
        render_index(serde_json::from_slice(&raw)?, &ids, now)
    }
}

/// One Algolia query for a known set of stories.
///
/// The tag syntax is comma = AND, parentheses = OR, so this asks for
/// *(story_a OR story_b OR …) AND story*. The trailing `story` is not
/// decoration: `story_<id>` is on the story **and on every one of its
/// comments**, and a front page of 30 stories carries thousands of those --
/// 2,509 in the capture that found this. Without it the request is one page of
/// 30 hits out of thousands, and which 30 come back is Algolia's business, not
/// ours: stories go missing from the front page for no visible reason. With it,
/// `hitsPerPage` is exact and every hit is a story.
fn batch_url(ids: &[String]) -> String {
    let tags = ids.iter().map(|id| format!("story_{id}"))
                  .collect::<Vec<String>>().join(",");
    format!("https://hn.algolia.com/api/v1/search?tags=({tags}),story&hitsPerPage={}",
            ids.len())
}

#[derive(Deserialize)]
struct Search {
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "objectID")]
    object_id: String,
    title: Option<String>,
    url: Option<String>,
    points: Option<i64>,
    num_comments: Option<i64>,
    author: Option<String>,
    created_at_i: Option<i64>,
}

/// One node of a thread. The same shape describes the story and its comments,
/// which is why `title`, `url` and `text` are all optional: a story has the
/// first two, a comment has the third, and a deleted comment has none of them
/// while still carrying replies.
#[derive(Deserialize)]
struct Item {
    title: Option<String>,
    url: Option<String>,
    text: Option<String>,
    author: Option<String>,
    points: Option<i64>,
    created_at_i: Option<i64>,
    #[serde(default)]
    children: Vec<Item>,
}

/// `order` is Hacker News's ranking. When it is empty the search's own order
/// stands, which is what a plain `render` call gets.
fn render_index(search: Search, order: &[String], now: i64) -> Result<Page, Error> {
    let mut body = String::with_capacity(8 * 1024);

    let ranked: Vec<&Hit> = if order.is_empty() {
        search.hits.iter().collect()
    } else {
        let found: FxHashMap<&str, &Hit> = search.hits.iter()
                                                 .map(|hit| (hit.object_id.as_str(), hit))
                                                 .collect();
        // An id with no hit is skipped rather than rendered blank. Two things
        // cause it, and neither is an error: a **job post**, which YC ranks on
        // the front page but Algolia does not index as a story at all, and a
        // brand-new item the index has not caught up with -- it trails YC by a
        // minute or two. So a front page of 30 ids routinely renders 29.
        order.iter().filter_map(|id| found.get(id.as_str()).copied()).collect()
    };

    for hit in &ranked {
        let title = hit.title.as_deref().unwrap_or("(untitled)");
        let thread = route_uri(ID, &Route::Thread(hit.object_id.clone()));

        body.push_str("<div class=\"story\">");
        let _ = write!(body, "<div class=\"headline\"><a href=\"{}\">{}</a></div>",
                       escape_attribute(&thread), escape_text(title));

        body.push_str("<div class=\"meta\">");
        let mut meta = Vec::new();
        if let Some(points) = hit.points {
            meta.push(format!("{points} points"));
        }
        if let Some(author) = hit.author.as_deref() {
            meta.push(escape_text(author));
        }
        if let Some(created) = hit.created_at_i {
            meta.push(relative_time(created, now));
        }
        let comments = hit.num_comments.unwrap_or(0);
        meta.push(format!("<a href=\"{}\">{comments} comment{}</a>",
                          escape_attribute(&thread), if comments == 1 { "" } else { "s" }));
        // The article itself is the *secondary* link -- the discussion is
        // what we came for -- but it no longer leaves this reader: the view
        // opens it through the article source.
        if let Some(url) = hit.url.as_deref() {
            meta.push(format!("<a href=\"{}\">{}</a>",
                              escape_attribute(url), escape_text(host_of(url))));
        }
        body.push_str(&meta.join(" · "));
        body.push_str("</div></div>");
    }

    if ranked.is_empty() {
        body.push_str("<p class=\"empty\">The front page came back empty.</p>");
    }

    Ok(Page::text("Hacker News".to_string(), body))
}

fn render_thread(story: Item, now: i64) -> Result<Page, Error> {
    let title = story.title.clone().unwrap_or_else(|| "Hacker News".to_string());
    let mut body = String::with_capacity(64 * 1024);

    body.push_str("<div class=\"head\">");
    match story.url.as_deref() {
        Some(url) => {
            let _ = write!(body, "<h1><a href=\"{}\">{}</a></h1>",
                           escape_attribute(url), escape_text(&title));
        }
        None => {
            let _ = write!(body, "<h1>{}</h1>", escape_text(&title));
        }
    }

    let mut meta = Vec::new();
    if let Some(points) = story.points {
        meta.push(format!("{points} points"));
    }
    if let Some(author) = story.author.as_deref() {
        meta.push(escape_text(author));
    }
    if let Some(created) = story.created_at_i {
        meta.push(relative_time(created, now));
    }
    if let Some(url) = story.url.as_deref() {
        meta.push(escape_text(host_of(url)));
    }
    let count = count_comments(&story.children);
    meta.push(format!("{count} comment{}", if count == 1 { "" } else { "s" }));
    let _ = write!(body, "<div class=\"meta\">{}</div>", meta.join(" · "));

    // An Ask HN or a text post carries its body on the story itself.
    if let Some(text) = story.text.as_deref().filter(|t| !t.trim().is_empty()) {
        let _ = write!(body, "<div class=\"storytext\">{}</div>", sanitize_fragment(text));
    }
    body.push_str("</div>");

    for child in &story.children {
        render_comment(&mut body, child, 0, now);
    }

    if count == 0 {
        body.push_str("<p class=\"empty\">No comments yet.</p>");
    }

    Ok(Page::text(title, body))
}

fn render_comment(body: &mut String, item: &Item, depth: usize, now: i64) {
    let text = item.text.as_deref().unwrap_or("").trim();

    // A deleted or flagged comment arrives with no author and no text, and
    // often still has replies. Rendering "[deleted]" would spend a line of a
    // small screen on nothing, so the node goes and the replies stay -- at the
    // same depth, since the thing they were replying to is gone too.
    if !text.is_empty() || item.author.is_some() {
        let _ = write!(body, "<div class=\"comment d{}\">", depth.min(MAX_DEPTH));

        let mut meta = Vec::new();
        if let Some(author) = item.author.as_deref() {
            meta.push(escape_text(author));
        }
        if let Some(created) = item.created_at_i {
            meta.push(relative_time(created, now));
        }
        if !meta.is_empty() {
            let _ = write!(body, "<div class=\"meta\">{}</div>", meta.join(" · "));
        }

        body.push_str(&sanitize_fragment(text));
        body.push_str("</div>");
    }

    let child_depth = if text.is_empty() && item.author.is_none() { depth } else { depth + 1 };
    for child in &item.children {
        render_comment(body, child, child_depth, now);
    }
}

fn count_comments(items: &[Item]) -> usize {
    items.iter().map(|item| 1 + count_comments(&item.children)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::html::xml::XmlParser;

    const NOW: i64 = 1_786_800_092;

    fn thread_json() -> String {
        // Hand-written rather than a captured thread: every awkward case this
        // renderer knows about is in here on purpose, which no real capture
        // guarantees. Field names and nesting are Algolia's.
        format!(r#"{{
          "id": 1, "title": "A story", "url": "https://www.example.com/a/b",
          "author": "alice", "points": 120, "created_at_i": {},
          "children": [
            {{"id": 2, "author": "bob", "created_at_i": {},
              "text": "first<p>second<p><pre><code>fn x() {{}}</code></pre>",
              "children": [
                {{"id": 3, "author": "carol", "created_at_i": {},
                  "text": "a &lt; b, see <a href=\"https://x.test/?a=1&amp;b=2\">this</a>",
                  "children": []}}
              ]}},
            {{"id": 4, "author": null, "text": null, "created_at_i": {},
              "children": [
                {{"id": 5, "author": "dave", "created_at_i": {},
                  "text": "orphaned reply", "children": []}}
              ]}}
          ]}}"#,
          NOW - 7_200, NOW - 3_600, NOW - 1_800, NOW - 3_000, NOW - 2_000)
    }

    fn render(route: Route, json: &str) -> Page {
        HackerNews.render(&route, json.as_bytes(), NOW).unwrap()
    }

    #[test]
    fn thread_urls_are_built_from_digits_only() {
        assert_eq!(HackerNews.url(&Route::Thread("49299605".into())).unwrap(),
                   "https://hn.algolia.com/api/v1/items/49299605");
        // The id reaches a URL, so it is checked rather than trusted.
        assert!(HackerNews.url(&Route::Thread("../search?q=x".into())).is_err());
    }

    #[test]
    fn a_thread_renders_story_then_comments() {
        let page = render(Route::Thread("1".into()), &thread_json());
        assert_eq!(page.title, "A story");
        assert!(page.body.contains("<h1><a href=\"https://www.example.com/a/b\">A story</a></h1>"));
        assert!(page.body.contains("120 points · alice · 2h · example.com · 4 comments"));
        assert!(page.body.contains("first<p>second</p>"), "{}", page.body);
        assert!(page.body.contains("<pre><code>fn x() {}</code></pre>"));
    }

    #[test]
    fn replies_are_indented_by_depth_and_deleted_nodes_do_not_indent() {
        let page = render(Route::Thread("1".into()), &thread_json());
        assert!(page.body.contains("<div class=\"comment d0\"><div class=\"meta\">bob"));
        assert!(page.body.contains("<div class=\"comment d1\"><div class=\"meta\">carol"));
        // dave replied to a deleted comment: he stays where it was, at d0.
        assert!(page.body.contains("<div class=\"comment d0\"><div class=\"meta\">dave"));
        // ... and the deleted node itself is not drawn at all.
        assert!(!page.body.contains("null"));
    }

    #[test]
    fn a_rendered_thread_parses_as_xml() {
        let page = render(Route::Thread("1".into()), &thread_json());
        let doc = format!("<body>{}</body>", page.body);
        let tree = XmlParser::new(&doc).parse();
        assert!(tree.root().text().contains("orphaned reply"));
    }

    #[test]
    fn the_index_links_titles_to_the_discussion_and_hosts_outward() {
        let json = format!(r#"{{"hits": [
            {{"objectID": "42", "title": "Something happened",
              "url": "https://www.pcworld.com/article/1/x.html",
              "points": 1287, "num_comments": 486, "author": "DemiGuru",
              "created_at_i": {}}},
            {{"objectID": "43", "title": "Ask HN: how?", "url": null,
              "points": 3, "num_comments": 0, "author": "eve", "created_at_i": {}}}
          ]}}"#, NOW - 10_800, NOW - 120);
        let page = render(Route::Index, &json);

        assert!(page.body.contains("<a href=\"news:hn/thread/42\">Something happened</a>"));
        assert!(page.body.contains("1287 points · DemiGuru · 3h · \
                                    <a href=\"news:hn/thread/42\">486 comments</a> · \
                                    <a href=\"https://www.pcworld.com/article/1/x.html\">pcworld.com</a>"),
                "{}", page.body);
        // A story with no article link shows no host link, and no empty one.
        assert!(page.body.contains("<a href=\"news:hn/thread/43\">0 comments</a>"));
        assert!(!page.body.contains("href=\"\""));
    }

    #[test]
    fn an_empty_front_page_says_so_rather_than_rendering_nothing() {
        let page = render(Route::Index, r#"{"hits": []}"#);
        assert!(page.body.contains("came back empty"));
    }
}
