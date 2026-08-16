//! Reading a few known sites, on the reader itself.
//!
//! The shape is the dictionary's -- a top bar, a page of laid-out HTML, a
//! bottom bar -- because the job is the same: render a fragment we generated
//! into the same engine, and page through it. What differs is where the
//! fragment comes from, and that difference is the whole design:
//!
//! * The markup is ours (`crate::news`) -- built from structured data, or,
//!   for an article opened from a link, extracted by the injected readability
//!   engine and scrubbed down to our vocabulary by the sanitizer. Either way
//!   nothing reaches the layout engine that `news` did not write or scrub;
//!   this is still not a browser, because nothing here lays out a stranger's
//!   page as that stranger designed it.
//! * The fetch happens on a worker thread. A page is one or two HTTPS requests
//!   over a radio that takes seconds to wake, and the event loop cannot wait
//!   for that -- so `load` spawns, and the answer arrives as `Event::NewsLoaded`.
//! * A link is ours (`news:<source>/thread/<id>`, followed here), or an
//!   http(s) article (opened right here through the hidden article source), or
//!   something only another machine can use -- `mailto:` and friends, queued
//!   to `external_urls_queue` exactly as the reader already does with such
//!   links in an EPUB. Nothing else is navigable.

mod bottom_bar;
mod tool_bar;

use std::sync::Arc;
use std::thread;

use fxhash::FxHashMap;

use crate::chrono::Local;
use crate::color::BLACK;
use crate::context::Context;
use crate::device::CURRENT_DEVICE;
use crate::document::html::HtmlDocument;
use crate::document::{Document, Location};
use crate::framebuffer::{Framebuffer, Pixmap, UpdateMode};
use crate::font::Fonts;
use crate::geom::{halves, CycleDir, Dir, Point, Rectangle};
use crate::gesture::GestureEvent;
use crate::input::{ButtonCode, ButtonStatus, DeviceEvent, FingerStatus};
use crate::news::{self, article, article::ArticleSource, feed::Feed, hn::HackerNews};
use crate::news::{ArticleExtractor, HttpClient, Route, Source};
use crate::settings::NewsSettings;
use crate::unit::scale_by_dpi;
use crate::view::common::locate_by_id;
use crate::view::common::{toggle_battery_menu, toggle_clock_menu, toggle_main_menu};
use crate::view::filler::Filler;
use crate::view::image::Image;
use crate::view::menu::{Menu, MenuKind};
use crate::view::notification::Notification;
use crate::view::top_bar::TopBar;
use crate::view::{Bus, Event, Hub, Id, RenderData, RenderQueue, SliderId, View, ViewId, ID_FEEDER};
use crate::view::{EntryId, EntryKind, SMALL_BAR_HEIGHT, THICKNESS_MEDIUM};
use self::bottom_bar::BottomBar;
use self::tool_bar::ToolBar;

const VIEWER_STYLESHEET: &str = "css/news.css";
const USER_STYLESHEET: &str = "css/news-user.css";


pub struct News {
    id: Id,
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    doc: HtmlDocument,
    location: usize,
    sources: Vec<Box<dyn Source>>,
    current: usize,
    route: Route,
    /// Where Back goes: the routes walked to get here, innermost last. A thread
    /// entered from the index leaves the index on this stack, so leaving the
    /// thread does not re-fetch it -- the rendered body is kept with it.
    history: Vec<HistoryEntry>,
    http: Arc<dyn HttpClient>,
    /// Kept alongside `sources` because the worker-thread copy in
    /// [`sources_for`] rebuilds the article source around this same engine --
    /// an `Arc` clone, where a feed is rebuilt from its three strings.
    extractor: Arc<dyn ArticleExtractor>,
    /// The markup currently shown, kept because `HtmlDocument` does not hand
    /// it back and going back must not re-fetch.
    body: String,
    /// The images behind that markup, kept for the same reason: a history
    /// entry that saved the body without them would come back with holes
    /// where the article's figures were.
    images: FxHashMap<String, Vec<u8>>,
    blurb_chars: usize,
    /// A route waiting for the radio, resumed on `NetUp`. See [`News::load`].
    pending: Option<Route>,
}

/// One step of Back: everything needed to re-show the page that was left
/// without a request. The body's images come too -- an article's `<img>`
/// srcs point into this map, so a kept body without it would re-show with
/// holes where the figures were.
struct HistoryEntry {
    source: usize,
    route: Route,
    body: String,
    images: FxHashMap<String, Vec<u8>>,
    location: usize,
}

/// Hacker News, then whatever `Settings.toml` names. Takes the settings rather
/// than the whole `Context` so the list can be built in a test.
fn sources(settings: &NewsSettings) -> Vec<Box<dyn Source>> {
    let mut sources: Vec<Box<dyn Source>> = vec![Box::new(HackerNews)];
    for feed in &settings.feeds {
        sources.push(Box::new(Feed::new(&feed.id, &feed.title, &feed.url)
                                  .with_blurb_chars(settings.blurb_chars)));
    }
    sources
}

impl News {
    pub fn new(rect: Rectangle, http: Arc<dyn HttpClient>, extractor: Arc<dyn ArticleExtractor>,
               hub: &Hub, rq: &mut RenderQueue, context: &mut Context) -> News {
        let id = ID_FEEDER.next();
        let mut children = Vec::new();
        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let (small_thickness, big_thickness) = halves(thickness);

        let mut sources = sources(&context.settings.news);
        // Last and hidden: the article source answers link taps, not the
        // source menu, and it has no front page to switch to.
        sources.push(Box::new(ArticleSource::new(Arc::clone(&extractor))));
        let name = sources[0].title().to_string();

        let top_bar = TopBar::new(rect![rect.min.x, rect.min.y,
                                        rect.max.x, rect.min.y + small_height - small_thickness],
                                  Event::NewsBack,
                                  name.clone(),
                                  context);
        children.push(Box::new(top_bar) as Box<dyn View>);

        let separator = Filler::new(rect![rect.min.x, rect.min.y + small_height - small_thickness,
                                          rect.max.x, rect.min.y + small_height + big_thickness],
                                    BLACK);
        children.push(Box::new(separator) as Box<dyn View>);

        let image_rect = rect![rect.min.x, rect.min.y + small_height + big_thickness,
                               rect.max.x, rect.max.y - small_height - small_thickness];
        children.push(Box::new(Image::new(image_rect, Pixmap::new(1, 1, 1))) as Box<dyn View>);

        let separator = Filler::new(rect![rect.min.x, rect.max.y - small_height - small_thickness,
                                          rect.max.x, rect.max.y - small_height + big_thickness],
                                    BLACK);
        children.push(Box::new(separator) as Box<dyn View>);

        let bottom_bar = BottomBar::new(rect![rect.min.x, rect.max.y - small_height + big_thickness,
                                              rect.max.x, rect.max.y],
                                        &name, false, false);
        children.push(Box::new(bottom_bar) as Box<dyn View>);

        let mut doc = HtmlDocument::new_from_memory("");
        doc.layout(image_rect.width(), image_rect.height(), context.settings.news.font_size, dpi);
        doc.set_margin_width(context.settings.news.margin_width);
        doc.set_viewer_stylesheet(VIEWER_STYLESHEET);
        doc.set_user_stylesheet(USER_STYLESHEET);
        // An article's photographs deserve better than 16 bare gray levels:
        // error diffusion, once per image at final scale (framebuffer::dither).
        doc.set_image_dither(true);

        rq.add(RenderData::new(id, rect, UpdateMode::Gui));

        let mut news = News {
            id,
            rect,
            children,
            doc,
            location: 0,
            sources,
            current: 0,
            route: Route::Index,
            body: String::new(),
            images: FxHashMap::default(),
            history: Vec::new(),
            blurb_chars: context.settings.news.blurb_chars,
            pending: None,
            http,
            extractor,
        };

        news.load(Route::Index, hub, rq, context);
        news
    }

    /// What the page says while the request is out. It is derived from the
    /// route rather than passed in, so the message is the same wherever the
    /// load was started from -- including the resume after WiFi comes up,
    /// which has no caller to pass it.
    fn loading_label(&self) -> String {
        match self.route {
            Route::Index => format!("Loading {}…",
                                    news::escape_text(self.sources[self.current].title())),
            // An article fetch is the slowest load this view makes -- an
            // arbitrary site instead of a JSON API -- so it earns a word.
            Route::Thread(_) if self.sources[self.current].id() == article::ID =>
                "Loading article…".to_string(),
            Route::Thread(_) => "Loading…".to_string(),
        }
    }

    /// Fetch on a worker thread and answer with an event. The radio is the slow
    /// part -- seconds, sometimes tens of them -- and everything the reader can
    /// still do meanwhile (turn a page, leave, suspend) it should still do.
    /// There is deliberately no in-flight guard. One would stop a second tap
    /// from queueing work, but it also strands the view on "Loading…" when you
    /// leave a page whose fetch has not come back -- and the answer to a
    /// question nobody is asking any more is already dropped on arrival, which
    /// is the hazard that actually matters.
    ///
    /// The radio comes first. This device rests with WiFi off, so the ordinary
    /// way to open News is offline -- and a request made then does not fail, it
    /// spends the whole 30 s timeout first. So ask for the radio, say so, and
    /// keep the route until `NetUp` says it can go.
    fn load(&mut self, route: Route, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        // Whatever was waiting for the radio, this supersedes it.
        self.pending = None;

        if !context.online {
            self.pending = Some(route);
            // `settings.wifi` already true with `online` still false means a
            // bring-up is in flight -- at startup, or from another view. Asking
            // again would be a no-op, so just wait for the same event.
            let message = if context.settings.wifi {
                "Waiting for WiFi…"
            } else {
                hub.send(Event::SetWifi(true)).ok();
                "Turning WiFi on…"
            };
            self.show(&format!("<p class=\"info\">{message}</p>"), FxHashMap::default(), rq);
            return;
        }

        let label = self.loading_label();
        self.show(&format!("<p class=\"info\">{label}</p>"), FxHashMap::default(), rq);

        let source_id = self.sources[self.current].id().to_string();
        let source = self.current;
        let http = Arc::clone(&self.http);
        let sources = sources_for(&self.sources, source, self.blurb_chars, &self.extractor);
        let hub = hub.clone();
        let now = Local::now().timestamp();

        thread::spawn(move || {
            let event = match news::load(sources.as_ref(), &route, http.as_ref(), now) {
                Ok(page) => Event::NewsLoaded {
                    source: source_id,
                    route,
                    page: Box::new(page),
                },
                // The message is the chain, because the useful half is usually
                // the cause: "the clock is wrong", "the name did not resolve".
                Err(err) => Event::NewsFailed(format!("{err:#}")),
            };
            hub.send(event).ok();
        });
    }

    /// The page being shown, as a history entry. The images move rather than
    /// clone -- they can be megabytes -- which is safe because every caller
    /// immediately loads another page, and `show` will restock the fields.
    fn here(&mut self) -> HistoryEntry {
        HistoryEntry {
            source: self.current,
            route: self.route.clone(),
            body: self.body.clone(),
            images: std::mem::take(&mut self.images),
            location: self.location,
        }
    }

    fn show(&mut self, body: &str, images: FxHashMap<String, Vec<u8>>, rq: &mut RenderQueue) {
        self.body = body.to_string();
        self.doc.set_resources(images.clone());
        self.images = images;
        self.doc.update(body);
        self.location = 0;
        if let Some(image) = self.children[2].downcast_mut::<Image>() {
            if let Some((pixmap, loc)) = self.doc.pixmap(Location::Exact(0), 1.0,
                                                         CURRENT_DEVICE.color_samples()) {
                image.update(pixmap, rq);
                self.location = loc;
            }
        }
        self.update_bottom_bar(rq);
    }

    fn update_bottom_bar(&mut self, rq: &mut RenderQueue) {
        let has_prev = self.doc.resolve_location(Location::Previous(self.location)).is_some();
        let has_next = self.doc.resolve_location(Location::Next(self.location)).is_some();
        if let Some(bottom_bar) = self.children[4].downcast_mut::<BottomBar>() {
            bottom_bar.update_icons(has_prev, has_next, rq);
        }
    }

    fn set_title(&mut self, title: &str, rq: &mut RenderQueue) {
        if let Some(top_bar) = self.children[0].downcast_mut::<TopBar>() {
            top_bar.update_title_label(title, rq);
        }
    }

    fn go_to_neighbor(&mut self, dir: CycleDir, rq: &mut RenderQueue) {
        let location = match dir {
            CycleDir::Previous => Location::Previous(self.location),
            CycleDir::Next => Location::Next(self.location),
        };
        if let Some(image) = self.children[2].downcast_mut::<Image>() {
            if let Some((pixmap, loc)) = self.doc.pixmap(location, 1.0,
                                                         CURRENT_DEVICE.color_samples()) {
                image.update(pixmap, rq);
                self.location = loc;
            }
        }
        self.update_bottom_bar(rq);
    }

    /// Follow what was tapped, or turn the page. Three kinds of link exist and
    /// only the first is navigation.
    fn follow_link(&mut self, pt: Point, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        // While a route is waiting for the radio there is nothing on the page
        // to tap -- no links, one line of text -- so a tap is a retry. It is
        // the escape hatch for a wait that outlived whatever it was waiting
        // for, which is a thing that can happen to any promise about a radio.
        if let Some(route) = self.pending.take() {
            self.load(route, hub, rq, context);
            return;
        }

        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let (_, big_thickness) = halves(thickness);
        let offset = pt!(self.rect.min.x, self.rect.min.y + small_height + big_thickness);

        let mut target = None;
        if let Some((links, _)) = self.doc.links(Location::Exact(self.location)) {
            // An exact hit on a link's own glyphs always wins: it is the only
            // way to reach the odd one out on a line -- the story's outbound
            // host link sits on the same line as its comment count.
            for link in &links {
                if (link.rect.to_rect() + offset).includes(pt) {
                    target = Some(link.text.clone());
                    break;
                }
            }

            // Otherwise fall back to the block the link belongs to. A story is
            // a run of consecutive links to the same place -- the headline,
            // over however many lines it wraps, and the comment count that
            // follows it -- so the union of their rectangles is the row, white
            // space and short last lines included. At 5.5 pt the glyphs are a
            // couple of millimetres tall; the row is a finger.
            if target.is_none() {
                let mut run: Option<(&str, Rectangle)> = None;
                for link in &links {
                    let next = link.rect.to_rect();
                    // Two links to the same place with a paragraph between them
                    // -- the same URL cited by two comments, say -- are not one
                    // block, and merging them would swallow everything in the
                    // gap. Only carry a run across a line break.
                    let contiguous = run.as_ref().is_some_and(|(_, rect)| {
                        next.min.y - rect.max.y <= 2 * next.height() as i32
                    });
                    match run {
                        Some((uri, ref mut rect)) if uri == link.text && contiguous => rect.absorb(&next),
                        _ => {
                            if let Some((uri, rect)) = run.take() {
                                if (rect + offset).includes(pt) {
                                    target = Some(uri.to_string());
                                    break;
                                }
                            }
                            run = Some((&link.text, next));
                        },
                    }
                }
                if target.is_none() {
                    if let Some((uri, rect)) = run {
                        if (rect + offset).includes(pt) {
                            target = Some(uri.to_string());
                        }
                    }
                }
            }
        }

        match target {
            Some(uri) => match news::parse_route_uri(&uri) {
                Some((_, route)) => self.go_to_route(route, hub, rq, context),
                None if uri.starts_with("http://") || uri.starts_with("https://") =>
                    self.open_article(uri, hub, rq, context),
                None => self.queue_external(&uri, hub, rq, context),
            },
            None => {
                let half_width = self.rect.width() as i32 / 2;
                if pt.x - offset.x < half_width {
                    self.go_to_neighbor(CycleDir::Previous, rq);
                } else {
                    self.go_to_neighbor(CycleDir::Next, rq);
                }
            }
        }
    }

    fn go_to_route(&mut self, route: Route, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        let entry = self.here();
        self.history.push(entry);
        self.route = route.clone();
        self.load(route, hub, rq, context);
    }

    /// An http(s) link opens as an article, right here: the hidden article
    /// source takes the URL as its route, and Back returns to the page the
    /// link was on, like any other step into `history`.
    fn open_article(&mut self, url: String, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        let Some(index) = self.sources.iter().position(|s| s.id() == article::ID) else {
            return self.queue_external(&url, hub, rq, context);
        };
        let entry = self.here();
        self.history.push(entry);
        self.current = index;
        self.route = Route::Thread(url);
        self.load(self.route.clone(), hub, rq, context);
    }

    /// Going back is free: the page that was left is kept with the history
    /// entry, so returning from a thread to the front page costs no request
    /// and no radio time.
    fn go_back(&mut self, rq: &mut RenderQueue) -> bool {
        let Some(HistoryEntry { source, route, body, images, location }) = self.history.pop() else {
            return false;
        };
        // Leaving the page a route was waiting for cancels the wait: the answer
        // would land on a page nobody is looking at.
        self.pending = None;
        self.current = source;
        self.route = route;
        let title = self.sources[source].title().to_string();
        self.set_title(&title, rq);
        if let Some(bottom_bar) = self.children[4].downcast_mut::<BottomBar>() {
            bottom_bar.update_name(&title, rq);
        }
        self.show(&body, images, rq);
        // `show` starts at the top; the position that was left is better.
        if let Some(image) = self.children[2].downcast_mut::<Image>() {
            if let Some((pixmap, loc)) = self.doc.pixmap(Location::Exact(location), 1.0,
                                                         CURRENT_DEVICE.color_samples()) {
                image.update(pixmap, rq);
                self.location = loc;
            }
        }
        self.update_bottom_bar(rq);
        true
    }

    /// A link only another machine can use -- `mailto:`, mostly. Articles
    /// landed here too, back when this reader refused to fetch them; they now
    /// open in-reader through the article source, and this queue keeps only
    /// what genuinely cannot be read on e-ink. The URL goes where an external
    /// link in an EPUB already goes, and the Mac deals with it.
    fn queue_external(&mut self, url: &str, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        use std::fs::OpenOptions;
        use std::io::Write;

        let message = match context.settings.external_urls_queue.as_ref() {
            // `create` makes the file, not the directory above it, and the
            // configured path points inside the article fetcher's directory --
            // which does not exist until that fetcher has been installed.
            Some(path) => path.parent()
                              .map_or(Ok(()), std::fs::create_dir_all)
                              .and_then(|_| OpenOptions::new().create(true).append(true).open(path))
                              .and_then(|mut file| writeln!(file, "{url}"))
                              .map_or_else(|e| format!("Couldn't queue {}: {e}.",
                                                       path.display()),
                                           |_| format!("Queued {}.", news::host_of(url))),
            None => "No external URL queue is configured.".to_string(),
        };
        let notif = Notification::new(message, hub, rq, context);
        self.children.push(Box::new(notif) as Box<dyn View>);
    }

    fn set_source(&mut self, id: &str, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        let Some(index) = self.sources.iter().position(|s| s.id() == id) else {
            return;
        };
        if index == self.current && matches!(self.route, Route::Index) {
            return;
        }

        self.current = index;
        self.route = Route::Index;
        self.history.clear();
        let title = self.sources[index].title().to_string();
        self.set_title(&title, rq);
        if let Some(bottom_bar) = self.children[4].downcast_mut::<BottomBar>() {
            bottom_bar.update_name(&title, rq);
        }
        self.load(Route::Index, hub, rq, context);
    }

    /// Show or hide the font size slider, floating over the foot of the page.
    fn toggle_tool_bar(&mut self, enable: Option<bool>, rq: &mut RenderQueue, context: &mut Context) {
        if let Some(index) = locate_by_id(self, ViewId::NewsToolBar) {
            if let Some(true) = enable {
                return;
            }
            rq.add(RenderData::expose(*self.child(index).rect(), UpdateMode::Gui));
            self.children.remove(index);
        } else {
            if let Some(false) = enable {
                return;
            }
            let dpi = CURRENT_DEVICE.dpi;
            let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
            let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
            let (small_thickness, _) = halves(thickness);
            // Sitting on the bottom bar's separator, so the two read as one
            // block of controls rather than a bar with a gap under it.
            let bottom = self.rect.max.y - small_height - small_thickness;
            let rect = rect![self.rect.min.x, bottom - ToolBar::height(),
                             self.rect.max.x, bottom];
            // The reader's range, not one of this view's own: 5.5 to 16.5 is
            // what "font size" means everywhere else in this app, and a news
            // page is the same text engine at the same dpi.
            let tool_bar = ToolBar::new(rect, context.settings.news.font_size,
                                        context.settings.reader.min_font_size,
                                        context.settings.reader.max_font_size);
            rq.add(RenderData::new(tool_bar.id(), *tool_bar.rect(), UpdateMode::Gui));
            self.children.push(Box::new(tool_bar) as Box<dyn View>);
        }
    }

    /// Re-lay out at a new size and stay where you were reading. The position
    /// is a byte offset into markup that has not changed, so it survives the
    /// relayout -- the same move `resize` makes when the geometry changes
    /// under a page.
    fn set_font_size(&mut self, font_size: f32, rq: &mut RenderQueue, context: &mut Context) {
        let font_size = font_size.clamp(context.settings.reader.min_font_size,
                                        context.settings.reader.max_font_size);
        if (font_size - context.settings.news.font_size).abs() < 0.05 {
            return;
        }
        // Kept in the settings, not in the view: Plato writes Settings.toml on
        // exit, so a size chosen here is the size the next session opens at.
        context.settings.news.font_size = font_size;

        let image_rect = *self.child(2).rect();
        self.doc.layout(image_rect.width(), image_rect.height(), font_size, CURRENT_DEVICE.dpi);
        if let Some(image) = self.children[2].downcast_mut::<Image>() {
            if let Some((pixmap, loc)) = self.doc.pixmap(Location::Exact(self.location), 1.0,
                                                         CURRENT_DEVICE.color_samples()) {
                image.update(pixmap, rq);
                self.location = loc;
            }
        }
        self.update_bottom_bar(rq);
    }

    fn toggle_source_menu(&mut self, rect: Rectangle, enable: Option<bool>, rq: &mut RenderQueue,
                          context: &mut Context) {
        if let Some(index) = locate_by_id(self, ViewId::NewsSourceMenu) {
            if let Some(true) = enable {
                return;
            }
            rq.add(RenderData::expose(*self.child(index).rect(), UpdateMode::Gui));
            self.children.remove(index);
        } else {
            if let Some(false) = enable {
                return;
            }
            // The article source is not on offer: switching to it would mean
            // asking it for a front page it does not have.
            let entries = self.sources.iter().enumerate()
                              .filter(|(_, source)| source.id() != article::ID)
                              .map(|(index, source)| {
                                  EntryKind::RadioButton(source.title().to_string(),
                                                         EntryId::SetNewsSource(source.id().to_string()),
                                                         index == self.current)
                              })
                              .collect::<Vec<EntryKind>>();
            let menu = Menu::new(rect, ViewId::NewsSourceMenu, MenuKind::DropDown, entries, context);
            rq.add(RenderData::new(menu.id(), *menu.rect(), UpdateMode::Gui));
            self.children.push(Box::new(menu) as Box<dyn View>);
        }
    }

    fn reseed(&mut self, rq: &mut RenderQueue, context: &mut Context) {
        if let Some(top_bar) = self.child_mut(0).downcast_mut::<TopBar>() {
            top_bar.reseed(rq, context);
        }
        rq.add(RenderData::new(self.id, self.rect, UpdateMode::Gui));
    }
}

/// The source a worker thread needs, as something it can own.
///
/// `Box<dyn Source>` in the view cannot cross a thread boundary by reference,
/// and the sources are cheap to reproduce -- an empty struct, three strings,
/// or, for the article source, another handle on the shared extractor -- so
/// the thread gets its own.
fn sources_for(sources: &[Box<dyn Source>], index: usize, blurb_chars: usize,
               extractor: &Arc<dyn ArticleExtractor>) -> Box<dyn Source> {
    let source = &sources[index];
    if source.id() == HackerNews.id() {
        Box::new(HackerNews)
    } else if source.id() == article::ID {
        // The extractor is machinery, not description: it is shared, and the
        // clone is of the `Arc`.
        Box::new(ArticleSource::new(Arc::clone(extractor)))
    } else {
        // Only feeds are configurable, and a feed is exactly (id, title, url).
        Box::new(Feed::new(source.id(), source.title(),
                           &source.url(&Route::Index).unwrap_or_default())
                     .with_blurb_chars(blurb_chars))
    }
}

impl View for News {
    fn handle_event(&mut self, evt: &Event, hub: &Hub, _bus: &mut Bus, rq: &mut RenderQueue,
                    context: &mut Context) -> bool {
        match *evt {
            Event::NewsLoaded { ref source, ref route, ref page } => {
                // A late answer to a question nobody is asking any more --
                // the source was switched while it was in flight.
                if source != self.sources[self.current].id() || *route != self.route {
                    return true;
                }
                let title = if matches!(route, Route::Index) {
                    self.sources[self.current].title().to_string()
                } else {
                    page.title.clone()
                };
                self.set_title(&title, rq);
                self.show(&page.body, page.images.clone(), rq);
                true
            },
            // The radio answered. `load` deferred a route rather than spend the
            // timeout offline; this is what it was waiting for.
            Event::Device(DeviceEvent::NetUp) => {
                if let Some(route) = self.pending.take() {
                    self.load(route, hub, rq, context);
                }
                true
            },
            Event::NetUpFailed => {
                if self.pending.take().is_some() {
                    self.show("<p class=\"info\">Couldn't load this page.</p>\
                               <p class=\"error\">WiFi didn't come up.</p>",
                              FxHashMap::default(), rq);
                }
                true
            },
            Event::NewsFailed(ref message) => {
                self.show(&format!("<p class=\"info\">Couldn't load this page.</p>\
                                    <p class=\"error\">{}</p>",
                                   news::escape_text(message)),
                          FxHashMap::default(), rq);
                true
            },
            Event::Page(dir) => {
                self.go_to_neighbor(dir, rq);
                true
            },
            Event::Gesture(GestureEvent::Swipe { dir, start, .. }) if self.rect.includes(start) => {
                match dir {
                    Dir::West => self.go_to_neighbor(CycleDir::Next, rq),
                    Dir::East => self.go_to_neighbor(CycleDir::Previous, rq),
                    _ => (),
                }
                true
            },
            Event::Device(DeviceEvent::Button { code, status: ButtonStatus::Released, .. }) => {
                let cd = match code {
                    ButtonCode::Backward => Some(CycleDir::Previous),
                    ButtonCode::Forward => Some(CycleDir::Next),
                    _ => None,
                };
                if let Some(cd) = cd {
                    let loc = self.location;
                    self.go_to_neighbor(cd, rq);
                    // At the end of a thread, the next page press leaves it --
                    // the same gesture the reader gives at the end of a book.
                    if self.location == loc && !self.go_back(rq) {
                        hub.send(Event::Back).ok();
                    }
                }
                true
            },
            // A tap anywhere else dismisses the slider, and does only that --
            // the same bargain a menu makes, so putting the bar away never
            // costs you a page turn or an article you did not mean to open.
            Event::Gesture(GestureEvent::Tap(center))
                    if self.rect.includes(center) &&
                       locate_by_id(self, ViewId::NewsToolBar).is_some() => {
                self.toggle_tool_bar(Some(false), rq, context);
                true
            },
            Event::Gesture(GestureEvent::Tap(center)) if self.rect.includes(center) => {
                self.follow_link(center, hub, rq, context);
                true
            },
            // Two fingers, because one is already spoken for: every single tap
            // on this page is either a link or a page turn, and there is no
            // corner left to spare. A two-finger tap collides with nothing, so
            // it is taken anywhere on the page -- a middle region would only be
            // something to miss -- and the menu opens between the fingers.
            Event::Gesture(GestureEvent::MultiTap(points)) => {
                let page = *self.child(2).rect();
                if !points.iter().all(|pt| page.includes(*pt)) {
                    return false;
                }
                self.toggle_tool_bar(None, rq, context);
                true
            },
            // Only on release. Every motion sample would otherwise repaginate
            // the whole page, which on this CPU is seconds of work per drag.
            Event::Slider(SliderId::FontSize, font_size, FingerStatus::Up) => {
                self.set_font_size(font_size, rq, context);
                true
            },
            Event::ToggleNear(ViewId::NewsToolBar, ..) => {
                self.toggle_tool_bar(None, rq, context);
                true
            },
            // The two ways out, and they mean the same thing: up one level,
            // and out of the app from the top level. The arrow is the one
            // people find; the gesture is for when the bars are hidden.
            Event::NewsBack | Event::Gesture(GestureEvent::Cross(_)) => {
                if !self.go_back(rq) {
                    hub.send(Event::Back).ok();
                }
                true
            },
            Event::Select(EntryId::SetNewsSource(ref id)) => {
                self.set_source(id, hub, rq, context);
                true
            },
            Event::ToggleNear(ViewId::NewsSourceMenu, rect) => {
                self.toggle_source_menu(rect, None, rq, context);
                true
            },
            Event::ToggleNear(ViewId::MainMenu, rect) => {
                toggle_main_menu(self, rect, None, rq, context);
                true
            },
            Event::ToggleNear(ViewId::BatteryMenu, rect) => {
                toggle_battery_menu(self, rect, None, rq, context);
                true
            },
            Event::ToggleNear(ViewId::ClockMenu, rect) => {
                toggle_clock_menu(self, rect, None, rq, context);
                true
            },
            Event::Reseed => {
                self.reseed(rq, context);
                true
            },
            _ => false,
        }
    }

    fn render(&self, _fb: &mut dyn Framebuffer, _rect: Rectangle, _fonts: &mut Fonts) {
    }

    fn resize(&mut self, rect: Rectangle, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let (small_thickness, big_thickness) = halves(thickness);

        self.children[0].resize(rect![rect.min.x, rect.min.y,
                                      rect.max.x, rect.min.y + small_height - small_thickness],
                                hub, rq, context);
        self.children[1].resize(rect![rect.min.x, rect.min.y + small_height - small_thickness,
                                      rect.max.x, rect.min.y + small_height + big_thickness],
                                hub, rq, context);

        let image_rect = rect![rect.min.x, rect.min.y + small_height + big_thickness,
                               rect.max.x, rect.max.y - small_height - small_thickness];
        self.doc.layout(image_rect.width(), image_rect.height(),
                        context.settings.news.font_size, dpi);
        if let Some(image) = self.children[2].downcast_mut::<Image>() {
            if let Some((pixmap, loc)) = self.doc.pixmap(Location::Exact(self.location), 1.0,
                                                         CURRENT_DEVICE.color_samples()) {
                image.update(pixmap, &mut RenderQueue::new());
                self.location = loc;
            }
        }
        self.children[2].resize(image_rect, hub, rq, context);

        self.children[3].resize(rect![rect.min.x, rect.max.y - small_height - small_thickness,
                                      rect.max.x, rect.max.y - small_height + big_thickness],
                                hub, rq, context);
        self.children[4].resize(rect![rect.min.x, rect.max.y - small_height + big_thickness,
                                      rect.max.x, rect.max.y],
                                hub, rq, context);
        self.update_bottom_bar(&mut RenderQueue::new());

        for index in 5..self.children.len() {
            self.children[index].resize(rect, hub, rq, context);
        }

        self.rect = rect;
        rq.add(RenderData::new(self.id, self.rect, UpdateMode::Gui));
    }

    fn rect(&self) -> &Rectangle {
        &self.rect
    }

    fn rect_mut(&mut self) -> &mut Rectangle {
        &mut self.rect
    }

    fn children(&self) -> &Vec<Box<dyn View>> {
        &self.children
    }

    fn children_mut(&mut self) -> &mut Vec<Box<dyn View>> {
        &mut self.children
    }

    fn id(&self) -> Id {
        self.id
    }

    fn view_id(&self) -> Option<ViewId> {
        Some(ViewId::News)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::news::Page;

    /// A source list is Hacker News plus whatever is configured, in that order,
    /// and the menu's radio state follows `current`.
    #[test]
    fn hacker_news_is_always_first() {
        let mut settings = NewsSettings::default();
        assert_eq!(sources(&settings).len(), 1);
        assert_eq!(sources(&settings)[0].id(), "hn");

        settings.feeds = vec![
            crate::settings::FeedSettings {
                id: "simonw".to_string(),
                title: "Simon Willison".to_string(),
                url: "https://simonwillison.net/atom/everything/".to_string(),
            },
        ];
        let sources = sources(&settings);
        assert_eq!(sources.len(), 2);
        // The article source is not configurable and not listed here: it is
        // appended, hidden, by `News::new`.
        assert!(sources.iter().all(|s| s.id() != article::ID));
        assert_eq!(sources[1].id(), "simonw");
        assert_eq!(sources[1].url(&Route::Index).unwrap(),
                   "https://simonwillison.net/atom/everything/");
    }

    /// A stand-in for `plato-article`, close enough for identity checks.
    struct Verbatim;

    impl ArticleExtractor for Verbatim {
        fn extract(&self, raw: &[u8], _url: &str) -> Result<crate::news::ExtractedArticle, anyhow::Error> {
            Ok(crate::news::ExtractedArticle {
                title: "t".to_string(),
                byline: None,
                site: None,
                html: String::from_utf8_lossy(raw).into_owned(),
            })
        }
    }

    /// The worker thread's copy has to be the same source, or it fetches the
    /// wrong site -- a feed's identity is its URL, and losing it here would be
    /// silent.
    #[test]
    fn a_source_survives_being_copied_for_a_thread() {
        let extractor: Arc<dyn ArticleExtractor> = Arc::new(Verbatim);
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(HackerNews),
            Box::new(Feed::new("verge", "The Verge", "https://www.theverge.com/rss/index.xml")),
            Box::new(ArticleSource::new(Arc::clone(&extractor))),
        ];

        let hn = sources_for(&sources, 0, 280, &extractor);
        assert_eq!(hn.id(), "hn");
        assert_eq!(hn.url(&Route::Thread("1".into())).unwrap(),
                   "https://hn.algolia.com/api/v1/items/1");

        let verge = sources_for(&sources, 1, 280, &extractor);
        assert_eq!(verge.id(), "verge");
        assert_eq!(verge.title(), "The Verge");
        assert_eq!(verge.url(&Route::Index).unwrap(), "https://www.theverge.com/rss/index.xml");

        // The copy is built around the same engine, and it still renders.
        let article = sources_for(&sources, 2, 280, &extractor);
        assert_eq!(article.id(), article::ID);
        let page = article.render(&Route::Thread("https://example.com/a".into()),
                                  b"<p>body</p>", 0).unwrap();
        assert!(page.body.contains("<p>body</p>"));
    }

    #[test]
    fn a_page_is_what_travels_in_the_event() {
        // Compiles only if `Page` can cross the channel, which is the whole
        // reason the fetch can leave the event loop.
        fn assert_send<T: Send>() {}
        assert_send::<Page>();
        assert_send::<Event>();
    }
}
