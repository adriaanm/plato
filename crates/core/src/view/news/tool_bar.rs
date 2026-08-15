//! One control, the reader's: font size on a horizontal slider.
//!
//! A transient row that floats over the bottom of the page rather than
//! displacing it, because it is opened for a moment and dismissed -- reflowing
//! the page to make room, the way the reader's tool bar does, would repaginate
//! twice for nothing.
//!
//! It replaced a menu of discrete sizes. The reader's font size menu offers
//! twenty-one steps of a tenth around the current value, which at 300 dpi
//! becomes a two-column block with its own "More" submenus, covering the very
//! page it is adjusting -- and a tenth of a point is not a difference anyone
//! can see in a list of headlines.

use crate::color::BLACK;
use crate::context::Context;
use crate::device::CURRENT_DEVICE;
use crate::font::Fonts;
use crate::framebuffer::Framebuffer;
use crate::geom::Rectangle;
use crate::gesture::GestureEvent;
use crate::input::DeviceEvent;
use crate::unit::scale_by_dpi;
use crate::view::filler::Filler;
use crate::view::icon::Icon;
use crate::view::slider::Slider;
use crate::view::{Bus, Event, Hub, Id, RenderQueue, SliderId, View, ViewId, ID_FEEDER};
use crate::view::{SMALL_BAR_HEIGHT, THICKNESS_MEDIUM};

pub struct ToolBar {
    id: Id,
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
}

impl ToolBar {
    /// `rect` is the whole row, separator included: see [`ToolBar::height`].
    pub fn new(rect: Rectangle, font_size: f32, min_font_size: f32, max_font_size: f32) -> ToolBar {
        let id = ID_FEEDER.next();
        let mut children = Vec::new();
        let dpi = CURRENT_DEVICE.dpi;
        let thickness = scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32;
        let side = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;

        // Its own top border, rather than a sibling the way the reader does it:
        // this bar is pushed and popped as one child, and a separator that can
        // be left behind is a line across the middle of the page.
        let separator = Filler::new(rect![rect.min.x, rect.min.y,
                                          rect.max.x, rect.min.y + thickness],
                                    BLACK);
        children.push(Box::new(separator) as Box<dyn View>);

        // The same icon the bottom bar opens this with, so what you tapped and
        // what appeared are visibly the same control.
        let icon_rect = rect![rect.min.x, rect.min.y + thickness,
                              rect.min.x + side, rect.max.y];
        let icon = Icon::new("font_size", icon_rect,
                             Event::ToggleNear(ViewId::NewsToolBar, icon_rect));
        children.push(Box::new(icon) as Box<dyn View>);

        let slider = Slider::new(rect![rect.min.x + side, rect.min.y + thickness,
                                       rect.max.x, rect.max.y],
                                 SliderId::FontSize,
                                 font_size, min_font_size, max_font_size);
        children.push(Box::new(slider) as Box<dyn View>);

        ToolBar { id, rect, children }
    }

    /// The row plus its border, which the view needs before it can place it.
    pub fn height() -> i32 {
        let dpi = CURRENT_DEVICE.dpi;
        scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32 + scale_by_dpi(THICKNESS_MEDIUM, dpi) as i32
    }
}

impl View for ToolBar {
    fn handle_event(&mut self, evt: &Event, _hub: &Hub, _bus: &mut Bus, _rq: &mut RenderQueue,
                    _context: &mut Context) -> bool {
        // Swallow what lands on the bar and nothing else: the slider's own
        // finger events reach it first (children are offered events before
        // their parent), and `Event::Slider` has to keep travelling up to the
        // news view, which is what actually knows how to re-lay out a page.
        match *evt {
            Event::Gesture(GestureEvent::Tap(center)) |
            Event::Gesture(GestureEvent::HoldFingerShort(center, ..)) if self.rect.includes(center) => true,
            Event::Device(DeviceEvent::Finger { position, .. }) if self.rect.includes(position) => true,
            _ => false,
        }
    }

    fn render(&self, _fb: &mut dyn Framebuffer, _rect: Rectangle, _fonts: &mut Fonts) {
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
        Some(ViewId::NewsToolBar)
    }
}
