//! RSS 2.0 and Atom, for skimming headlines and blurbs.
//!
//! Deliberately shallow: a headline, a line of metadata, and a blurb of a few
//! lines. Nothing here fetches an article, and nothing here extracts one from a
//! page -- that was the explicit scope decision, and it is what keeps this file
//! short. The article link leaves the reader through the existing external-URL
//! queue.
//!
//! The blurb is *text*, cut to `blurb_chars`. Some feeds put a whole article in
//! it -- one Verge entry filled the panel end to end -- which turns a list you
//! skim into a list of one, so the markup inside it is dropped rather than
//! rendered (`text_only`).
//!
//! Feeds are XML, so `document::html::xml` parses them as they stand -- the
//! same parser the reader already uses for EPUB.

use anyhow::Error;
use std::fmt::Write;

use crate::document::html::dom::NodeRef;
use crate::document::html::xml::XmlParser;
use crate::helpers::decode_entities;

use super::{escape_attribute, escape_text, host_of, relative_time, route_uri};
use super::{sanitize_fragment, text_only};
use super::{Page, Route, Source};

/// One configured feed. The list of these lives in `Settings.toml`, so adding
/// a site is an edit, not a build.
pub struct Feed {
    pub id: String,
    pub title: String,
    pub url: String,
    /// How much of a blurb to show. See `news.blurb-chars`.
    pub blurb_chars: usize,
}

/// About four lines at the default size. Measured rather than guessed: at 280
/// a Verge entry ran to six lines and three entries filled the panel; at 200 a
/// screen holds four or five, which is what makes the list skimmable.
pub const DEFAULT_BLURB_CHARS: usize = 200;

impl Feed {
    pub fn new(id: &str, title: &str, url: &str) -> Feed {
        Feed { id: id.to_string(), title: title.to_string(), url: url.to_string(),
               blurb_chars: DEFAULT_BLURB_CHARS }
    }

    pub fn with_blurb_chars(mut self, blurb_chars: usize) -> Feed {
        self.blurb_chars = blurb_chars;
        self
    }
}

impl Source for Feed {
    fn id(&self) -> &str {
        &self.id
    }

    fn title(&self) -> &str {
        &self.title
    }

    /// Both routes read the same document: a feed carries its entries with it,
    /// so opening one is a matter of finding it in what was already published,
    /// not of fetching an article. The cost is re-reading the feed -- tens of
    /// kilobytes, one request -- which is still cheaper than any page it links
    /// to, and it is the reason no article ever has to be fetched or parsed.
    fn url(&self, _route: &Route) -> Result<String, Error> {
        Ok(self.url.clone())
    }

    fn render(&self, route: &Route, raw: &[u8], now: i64) -> Result<Page, Error> {
        let text = String::from_utf8_lossy(raw);
        let tree = XmlParser::new(&text).parse();
        let root = tree.root();
        let entries = root.descendants()
                          .filter(|n| matches!(n.tag_name(), Some("item") | Some("entry")));

        if let Route::Thread(wanted) = route {
            for entry in entries {
                if entry_id(entry).as_deref() == Some(wanted.as_str()) {
                    return Ok(self.render_entry(entry, now));
                }
            }
            // The feed moved on while it was being read. Saying so beats an
            // empty page, and the index is one tap away.
            return Ok(Page::text(
                self.title.clone(),
                "<p class=\"info\">This entry is no longer in the feed.</p>".to_string()));
        }

        let mut body = String::with_capacity(8 * 1024);
        let mut count = 0;

        for entry in entries {
            count += 1;
            let title = child_text(entry, "title").unwrap_or_else(|| "(untitled)".to_string());
            let link = entry_link(entry);
            let full = entry_blurb(entry).unwrap_or_default();
            let blurb = text_only(&full, self.blurb_chars);

            // Where the headline goes. An entry the feed publishes in full is
            // worth opening here; one that was only ever a summary has nothing
            // more to show, so its headline is the article, which leaves the
            // reader through the external-URL queue.
            let target = entry_id(entry)
                             .filter(|_| blurb.ends_with('…'))
                             .map(|id| route_uri(&self.id, &Route::Thread(id)))
                             .or_else(|| link.clone());

            body.push_str("<div class=\"story\">");
            match target {
                Some(uri) => {
                    let _ = write!(body, "<div class=\"headline\"><a href=\"{}\">{}</a></div>",
                                   escape_attribute(&uri), escape_text(&title));
                }
                None => {
                    let _ = write!(body, "<div class=\"headline\">{}</div>", escape_text(&title));
                }
            }

            let _ = write!(body, "{}", self.meta_line(entry, link.as_deref(), now));

            if !blurb.is_empty() {
                let _ = write!(body, "<div class=\"blurb\">{blurb}</div>");
            }
            body.push_str("</div>");
        }

        if count == 0 {
            body.push_str("<p class=\"empty\">This feed has no entries.</p>");
        }

        // The feed's own title is nicer than the configured one when they
        // differ, but the configured one is what the user chose to call it.
        Ok(Page::text(self.title.clone(), body))
    }
}

impl Feed {
    fn meta_line(&self, entry: NodeRef<'_>, link: Option<&str>, now: i64) -> String {
        let mut meta = Vec::new();
        if let Some(published) = entry_time(entry) {
            meta.push(relative_time(published, now));
        }
        if let Some(author) = entry_author(entry) {
            meta.push(escape_text(&author));
        }
        if let Some(url) = link {
            meta.push(escape_text(host_of(url)));
        }
        if meta.is_empty() {
            String::new()
        } else {
            format!("<div class=\"meta\">{}</div>", meta.join(" · "))
        }
    }

    /// One entry, in full: what the feed published, with its markup kept.
    ///
    /// The article link is offered but not followed -- tapping it queues the
    /// URL, as everywhere else here. Nothing fetches a page.
    fn render_entry(&self, entry: NodeRef<'_>, now: i64) -> Page {
        let title = child_text(entry, "title").unwrap_or_else(|| "(untitled)".to_string());
        let link = entry_link(entry);
        let mut body = String::with_capacity(8 * 1024);

        body.push_str("<div class=\"head\">");
        match link.as_deref() {
            Some(url) => {
                let _ = write!(body, "<h1><a href=\"{}\">{}</a></h1>",
                               escape_attribute(url), escape_text(&title));
            }
            None => {
                let _ = write!(body, "<h1>{}</h1>", escape_text(&title));
            }
        }
        let _ = write!(body, "{}</div>", self.meta_line(entry, link.as_deref(), now));

        match entry_blurb(entry).map(|raw| sanitize_fragment(&raw))
                                .filter(|content| !content.trim().is_empty()) {
            Some(content) => {
                let _ = write!(body, "<div class=\"storytext\">{content}</div>");
            }
            None => body.push_str("<p class=\"info\">This entry has no text of its own.</p>"),
        }

        Page::text(title, body)
    }
}

/// What a route names an entry by. Both formats provide one; the link is the
/// fallback, and it is what most feeds use for the identifier anyway.
fn entry_id(entry: NodeRef<'_>) -> Option<String> {
    child_text(entry, "id")
        .or_else(|| child_text(entry, "guid"))
        .or_else(|| entry_link(entry))
}

fn child<'a>(node: NodeRef<'a>, name: &str) -> Option<NodeRef<'a>> {
    node.children().find(|c| c.tag_name() == Some(name))
}

/// The text of a child element, with **one** layer of entities removed --
/// which is the layer XML itself added.
///
/// Getting this wrong is visible either way. Leave it encoded and a title
/// reads `America&amp;#8217;s` on screen, because the renderer escapes what it
/// is given; decode it twice and a feed that legitimately says `&amp;lt;` loses
/// its text to the sanitiser. Once, here, and the HTML layer inside a blurb is
/// then decoded later by the layout engine, exactly like an HN comment body.
fn child_text(node: NodeRef<'_>, name: &str) -> Option<String> {
    child(node, name).map(|c| decode_entities(c.text().trim()).into_owned())
                     .filter(|t| !t.is_empty())
}

/// RSS puts the URL in `<link>`'s text; Atom puts it in a `href` attribute,
/// and may offer several with different `rel`s -- `alternate` (or no `rel` at
/// all, which means `alternate`) is the article.
fn entry_link(entry: NodeRef<'_>) -> Option<String> {
    for link in entry.children().filter(|c| c.tag_name() == Some("link")) {
        match link.attribute("href") {
            Some(href) => {
                if matches!(link.attribute("rel"), None | Some("alternate")) {
                    return Some(href.to_string());
                }
            }
            None => {
                let text = link.text().trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    child_text(entry, "guid").filter(|g| g.starts_with("http"))
}

fn entry_author(entry: NodeRef<'_>) -> Option<String> {
    child(entry, "author").and_then(|a| child_text(a, "name"))   // Atom
        .or_else(|| child_text(entry, "dc:creator"))             // RSS, commonly
        .or_else(|| child_text(entry, "author"))                 // RSS, an email
}

/// Both date formats, reduced to Unix seconds: RFC 822 in RSS `pubDate`, RFC
/// 3339 in Atom `updated`/`published`. Only ever used to print "3h", so a feed
/// with an unparseable date loses its age and keeps everything else.
fn entry_time(entry: NodeRef<'_>) -> Option<i64> {
    use crate::chrono::{DateTime, Utc};

    let rfc3339 = child_text(entry, "published")
                      .or_else(|| child_text(entry, "updated"))
                      .and_then(|t| DateTime::parse_from_rfc3339(&t).ok());
    let rfc2822 = || child_text(entry, "pubDate")
                        .and_then(|t| DateTime::parse_from_rfc2822(&t).ok());

    rfc3339.or_else(rfc2822)
           .map(|t| t.with_timezone(&Utc).timestamp())
}

/// The blurb, in the order of preference a reader would want: the full content
/// if the feed ships it, else the summary.
///
/// Both arrive *escaped* -- `&lt;p&gt;` -- because a feed carries HTML inside
/// XML, so this decodes one layer before sanitising. That is the one place in
/// this module where getting the order wrong shows literal tags on screen.
fn entry_blurb(entry: NodeRef<'_>) -> Option<String> {
    child_text(entry, "content")                         // Atom
        .or_else(|| child_text(entry, "content:encoded"))
        .or_else(|| child_text(entry, "summary"))
        .or_else(|| child_text(entry, "description"))    // RSS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::html::xml::XmlParser;

    const NOW: i64 = 1_786_800_092;   // 2026-08-15T13:21:32Z

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
      <feed xmlns="http://www.w3.org/2005/Atom">
        <title>Simon Willison's Weblog</title>
        <entry>
          <title>Something about LLMs</title>
          <link href="https://simonwillison.net/2026/Aug/15/something/" rel="alternate"/>
          <link href="https://simonwillison.net/2026/Aug/15/something/#comments" rel="replies"/>
          <published>2026-08-15T10:21:32+00:00</published>
          <author><name>Simon Willison</name></author>
          <summary>&lt;p&gt;A paragraph with &lt;a href="https://x.test/"&gt;a link&lt;/a&gt;.&lt;/p&gt;</summary>
        </entry>
      </feed>"#;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
      <rss version="2.0"><channel>
        <title>The Verge</title>
        <item>
          <title>A gadget was announced</title>
          <link>https://www.theverge.com/2026/8/15/gadget</link>
          <pubDate>Sat, 15 Aug 2026 11:21:32 +0000</pubDate>
          <description>&lt;p&gt;The blurb.&lt;/p&gt;&lt;img src="https://tracker.test/p.gif"&gt;</description>
        </item>
        <item>
          <title>No date, no blurb</title>
          <link>https://www.theverge.com/2026/8/15/other</link>
        </item>
      </channel></rss>"#;

    fn render(xml: &str) -> Page {
        Feed::new("verge", "The Verge", "https://example.test/feed")
            .render(&Route::Index, xml.as_bytes(), NOW).unwrap()
    }

    /// A feed that ships whole articles must not turn the list into a list of
    /// one -- the measured case is The Verge, whose `content` filled the panel.
    #[test]
    fn a_long_blurb_is_cut_to_a_few_lines() {
        let long = "word ".repeat(400);
        let xml = format!(r#"<rss version="2.0"><channel><item>
            <title>Long</title><link>https://x.test/a</link>
            <description>{long}</description></item></channel></rss>"#);
        let page = Feed::new("x", "X", "https://x.test/feed").with_blurb_chars(120)
                       .render(&Route::Index, xml.as_bytes(), NOW).unwrap();
        let blurb = page.body.split("class=\"blurb\">").nth(1).unwrap()
                        .split("</div>").next().unwrap();
        assert!(blurb.chars().count() <= 121, "{} chars", blurb.chars().count());
        assert!(blurb.ends_with('…'));
    }

    #[test]
    fn atom_entries_use_the_alternate_link_and_the_author_name() {
        let page = render(ATOM);
        assert!(page.body.contains("<a href=\"https://simonwillison.net/2026/Aug/15/something/\">\
                                    Something about LLMs</a>"), "{}", page.body);
        assert!(!page.body.contains("#comments"));
        assert!(page.body.contains("3h · Simon Willison · simonwillison.net"));
    }

    #[test]
    fn escaped_html_in_a_blurb_is_decoded_once_then_flattened() {
        let page = render(ATOM);
        // The blurb is text: the link inside it is gone (the headline is the
        // link), and dropping it neither glued nor split the sentence.
        assert!(page.body.contains("<div class=\"blurb\">A paragraph with a link.</div>"),
                "{}", page.body);
        // Not double-decoded, not left escaped: no literal tags on screen.
        assert!(!page.body.contains("&lt;p&gt;"));
    }

    #[test]
    fn rss_items_parse_and_tracking_images_do_not_survive() {
        let page = render(RSS);
        assert!(page.body.contains("2h · theverge.com"));
        assert!(page.body.contains("<div class=\"blurb\">The blurb.</div>"), "{}", page.body);
        assert!(!page.body.contains("tracker.test"));
    }

    #[test]
    fn an_entry_missing_everything_optional_still_renders() {
        let page = render(RSS);
        assert!(page.body.contains("No date, no blurb"));
        assert!(!page.body.contains("<div class=\"meta\"></div>"));
    }

    /// An entry the feed publishes in full opens here; one that was only ever
    /// a summary has nothing more to show, so its headline is the article.
    #[test]
    fn a_truncated_entry_is_openable_and_a_short_one_links_out() {
        let long = "word ".repeat(200);
        let xml = format!(r#"<feed xmlns="http://www.w3.org/2005/Atom">
            <entry><title>Long</title><id>tag:x,2026:1</id>
              <link href="https://x.test/long" rel="alternate"/>
              <content>&lt;p&gt;{long}&lt;/p&gt;</content></entry>
            <entry><title>Short</title><id>tag:x,2026:2</id>
              <link href="https://x.test/short" rel="alternate"/>
              <summary>Just a line.</summary></entry>
          </feed>"#);
        let feed = Feed::new("simonw", "Simon Willison", "https://x.test/feed");
        let page = feed.render(&Route::Index, xml.as_bytes(), NOW).unwrap();

        assert!(page.body.contains("<a href=\"news:simonw/thread/tag:x,2026:1\">Long</a>"),
                "{}", page.body);
        assert!(page.body.contains("<a href=\"https://x.test/short\">Short</a>"), "{}", page.body);

        // Opening it shows the whole entry, with its markup kept this time.
        let entry = feed.render(&Route::Thread("tag:x,2026:1".into()), xml.as_bytes(), NOW).unwrap();
        assert_eq!(entry.title, "Long");
        assert!(entry.body.contains("<h1><a href=\"https://x.test/long\">Long</a></h1>"));
        assert!(entry.body.contains("<div class=\"storytext\"><p>word word"), "{}", &entry.body[..200]);
        assert!(!entry.body.contains('…'));

        // And an entry that has since fallen off the feed says so.
        let gone = feed.render(&Route::Thread("tag:x,2026:9".into()), xml.as_bytes(), NOW).unwrap();
        assert!(gone.body.contains("no longer in the feed"));
    }

    #[test]
    fn a_rendered_feed_parses_as_xml() {
        for xml in [ATOM, RSS] {
            let page = render(xml);
            let doc = format!("<body>{}</body>", page.body);
            assert!(!XmlParser::new(&doc).parse().root().text().is_empty());
        }
    }

    #[test]
    fn an_empty_feed_says_so() {
        let page = render(r#"<rss version="2.0"><channel><title>x</title></channel></rss>"#);
        assert!(page.body.contains("no entries"));
    }
}
