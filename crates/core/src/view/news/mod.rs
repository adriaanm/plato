//! Reading a few known sites, on the reader itself.
//!
//! The shape is the dictionary's -- a top bar, a page of laid-out HTML, a
//! bottom bar -- because the job is the same: render a fragment we generated
//! into the same engine, and page through it. What differs is where the
//! fragment comes from, and that difference is the whole design:
//!
//! * The markup is ours (`crate::news`), so nothing here parses a stranger's
//!   page. This is not a browser and does not grow into one.
//! * The fetch happens on a worker thread. A page is one or two HTTPS requests
//!   over a radio that takes seconds to wake, and the event loop cannot wait
//!   for that -- so `load` spawns, and the answer arrives as `Event::NewsLoaded`.
//! * A link is either ours (`news:<source>/thread/<id>`, followed here) or the
//!   open web (queued to `external_urls_queue`, exactly as the reader already
//!   does with an external link in an EPUB). Nothing else is navigable.

mod bottom_bar;

use std::sync::Arc;
use std::thread;

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
use crate::input::{ButtonCode, ButtonStatus, DeviceEvent};
use crate::news::{self, feed::Feed, hn::HackerNews, HttpClient, Route, Source};
use crate::settings::NewsSettings;
use crate::unit::scale_by_dpi;
use crate::view::common::locate_by_id;
use crate::view::common::{toggle_battery_menu, toggle_clock_menu, toggle_main_menu};
use crate::view::filler::Filler;
use crate::view::image::Image;
use crate::view::menu::{Menu, MenuKind};
use crate::view::notification::Notification;
use crate::view::top_bar::TopBar;
use crate::view::{Bus, Event, Hub, Id, RenderData, RenderQueue, View, ViewId, ID_FEEDER};
use crate::view::{EntryId, EntryKind, SMALL_BAR_HEIGHT, THICKNESS_MEDIUM};
use self::bottom_bar::BottomBar;

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
    history: Vec<(usize, Route, String, usize)>,
    http: Arc<dyn HttpClient>,
    /// The markup currently shown, kept because `HtmlDocument` does not hand
    /// it back and going back must not re-fetch.
    body: String,
    blurb_chars: usize,
    /// A route waiting for the radio, resumed on `NetUp`. See [`News::load`].
    pending: Option<Route>,
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
    pub fn new(rect: Rectangle, http: Arc<dyn HttpClient>, hub: &Hub, rq: &mut RenderQueue,
               context: &mut Context) -> News {
        let id = ID_FEEDER.next();
        let mut children = Vec::new();
        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let (small_thickness, big_thickness) = halves(thickness);

        let sources = sources(&context.settings.news);
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
            history: Vec::new(),
            blurb_chars: context.settings.news.blurb_chars,
            pending: None,
            http,
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
            self.show(&format!("<p class=\"info\">{message}</p>"), rq);
            return;
        }

        let label = self.loading_label();
        self.show(&format!("<p class=\"info\">{label}</p>"), rq);

        let source_id = self.sources[self.current].id().to_string();
        let source = self.current;
        let http = Arc::clone(&self.http);
        let sources = sources_for(&self.sources, source, self.blurb_chars);
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

    fn show(&mut self, body: &str, rq: &mut RenderQueue) {
        self.body = body.to_string();
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
        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let (_, big_thickness) = halves(thickness);
        let offset = pt!(self.rect.min.x, self.rect.min.y + small_height + big_thickness);

        let mut target = None;
        if let Some((links, _)) = self.doc.links(Location::Exact(self.location)) {
            for link in links {
                if (link.rect.to_rect() + offset).includes(pt) {
                    target = Some(link.text.clone());
                    break;
                }
            }
        }

        match target {
            Some(uri) => match news::parse_route_uri(&uri) {
                Some((_, route)) => self.go_to_route(route, hub, rq, context),
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
        self.history.push((self.current, self.route.clone(),
                           self.body.clone(), self.location));
        self.route = route.clone();
        self.load(route, hub, rq, context);
    }

    /// Going back is free: the page that was left is kept with the history
    /// entry, so returning from a thread to the front page costs no request
    /// and no radio time.
    fn go_back(&mut self, rq: &mut RenderQueue) -> bool {
        let Some((source, route, body, location)) = self.history.pop() else {
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
        self.show(&body, rq);
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

    /// An article link. This reader does not fetch articles -- that was the
    /// scope decision, and it is what keeps `news` free of readability
    /// heuristics -- so the URL goes where an external link in an EPUB already
    /// goes, and the Mac deals with it.
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
            let entries = self.sources.iter().enumerate()
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
/// and the sources are cheap descriptions -- an empty struct, or three strings
/// -- so the thread gets its own.
fn sources_for(sources: &[Box<dyn Source>], index: usize, blurb_chars: usize) -> Box<dyn Source> {
    let source = &sources[index];
    if source.id() == HackerNews.id() {
        Box::new(HackerNews)
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
                self.show(&page.body, rq);
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
                               <p class=\"error\">WiFi didn't come up.</p>", rq);
                }
                true
            },
            Event::NewsFailed(ref message) => {
                self.show(&format!("<p class=\"info\">Couldn't load this page.</p>\
                                    <p class=\"error\">{}</p>",
                                   news::escape_text(message)), rq);
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
            Event::Gesture(GestureEvent::Tap(center)) if self.rect.includes(center) => {
                self.follow_link(center, hub, rq, context);
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
        assert_eq!(sources[1].id(), "simonw");
        assert_eq!(sources[1].url(&Route::Index).unwrap(),
                   "https://simonwillison.net/atom/everything/");
    }

    /// The worker thread's copy has to be the same source, or it fetches the
    /// wrong site -- a feed's identity is its URL, and losing it here would be
    /// silent.
    #[test]
    fn a_source_survives_being_copied_for_a_thread() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(HackerNews),
            Box::new(Feed::new("verge", "The Verge", "https://www.theverge.com/rss/index.xml")),
        ];

        let hn = sources_for(&sources, 0, 280);
        assert_eq!(hn.id(), "hn");
        assert_eq!(hn.url(&Route::Thread("1".into())).unwrap(),
                   "https://hn.algolia.com/api/v1/items/1");

        let verge = sources_for(&sources, 1, 280);
        assert_eq!(verge.id(), "verge");
        assert_eq!(verge.title(), "The Verge");
        assert_eq!(verge.url(&Route::Index).unwrap(), "https://www.theverge.com/rss/index.xml");
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
