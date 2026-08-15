//! Previous page, the source's name, next page.
//!
//! The dictionary's bottom bar with one difference: the label in the middle
//! opens the source menu rather than the dictionary target menu, so switching
//! from Hacker News to a feed is one tap from where you are reading.

use crate::color::WHITE;
use crate::context::Context;
use crate::font::Fonts;
use crate::framebuffer::{Framebuffer, UpdateMode};
use crate::geom::{CycleDir, Rectangle};
use crate::gesture::GestureEvent;
use crate::input::DeviceEvent;
use crate::view::filler::Filler;
use crate::view::icon::Icon;
use crate::view::label::Label;
use crate::view::{Align, Bus, Event, Hub, Id, RenderData, RenderQueue, View, ViewId, ID_FEEDER};

#[derive(Debug)]
pub struct BottomBar {
    id: Id,
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    has_prev: bool,
    has_next: bool,
}

impl BottomBar {
    pub fn new(rect: Rectangle, name: &str, has_prev: bool, has_next: bool) -> BottomBar {
        let id = ID_FEEDER.next();
        let mut children = Vec::new();
        let side = rect.height() as i32;

        let prev_rect = rect![rect.min, rect.min + side];
        children.push(page_child(prev_rect, has_prev, CycleDir::Previous));

        let name_rect = rect![pt!(rect.min.x + side, rect.min.y),
                              pt!(rect.max.x - side, rect.max.y)];
        let name_label = Label::new(name_rect, name.to_string(), Align::Center)
                               .event(Some(Event::ToggleNear(ViewId::NewsSourceMenu, name_rect)));
        children.push(Box::new(name_label) as Box<dyn View>);

        let next_rect = rect![rect.max - side, rect.max];
        children.push(page_child(next_rect, has_next, CycleDir::Next));

        BottomBar { id, rect, children, has_prev, has_next }
    }

    pub fn update_icons(&mut self, has_prev: bool, has_next: bool, rq: &mut RenderQueue) {
        if self.has_prev != has_prev {
            let rect = *self.child(0).rect();
            self.children[0] = page_child(rect, has_prev, CycleDir::Previous);
            self.has_prev = has_prev;
            rq.add(RenderData::new(self.id, rect, UpdateMode::Gui));
        }

        if self.has_next != has_next {
            let index = self.len() - 1;
            let rect = *self.child(index).rect();
            self.children[index] = page_child(rect, has_next, CycleDir::Next);
            self.has_next = has_next;
            rq.add(RenderData::new(self.id, rect, UpdateMode::Gui));
        }
    }

    pub fn update_name(&mut self, text: &str, rq: &mut RenderQueue) {
        if let Some(label) = self.child_mut(1).downcast_mut::<Label>() {
            label.update(text, rq);
        }
    }
}

/// An arrow when there is somewhere to go, and blank space when there is not --
/// an arrow that does nothing is worse than no arrow.
fn page_child(rect: Rectangle, enabled: bool, dir: CycleDir) -> Box<dyn View> {
    if enabled {
        let name = match dir {
            CycleDir::Previous => "arrow-left",
            CycleDir::Next => "arrow-right",
        };
        Box::new(Icon::new(name, rect, Event::Page(dir))) as Box<dyn View>
    } else {
        Box::new(Filler::new(rect, WHITE)) as Box<dyn View>
    }
}

impl View for BottomBar {
    fn handle_event(&mut self, evt: &Event, _hub: &Hub, _bus: &mut Bus, _rq: &mut RenderQueue,
                    _context: &mut Context) -> bool {
        match *evt {
            Event::Gesture(GestureEvent::Tap(center)) |
            Event::Gesture(GestureEvent::HoldFingerShort(center, ..)) if self.rect.includes(center) => true,
            Event::Device(DeviceEvent::Finger { position, .. }) if self.rect.includes(position) => true,
            _ => false,
        }
    }

    fn render(&self, _fb: &mut dyn Framebuffer, _rect: Rectangle, _fonts: &mut Fonts) {
    }

    fn resize(&mut self, rect: Rectangle, hub: &Hub, rq: &mut RenderQueue, context: &mut Context) {
        let side = rect.height() as i32;
        self.children[0].resize(rect![rect.min, rect.min + side], hub, rq, context);
        self.children[1].resize(rect![pt!(rect.min.x + side, rect.min.y),
                                      pt!(rect.max.x - side, rect.max.y)], hub, rq, context);
        self.children[2].resize(rect![rect.max - side, rect.max], hub, rq, context);
        self.rect = rect;
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
}
