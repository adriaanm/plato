//! The WiFi indicator in the top bar.
//!
//! Why this exists: enabling WiFi from the menu dismissed the menu and then
//! showed nothing at all until the "WiFi enabled" notification arrived. The
//! script blocks until associated and addressed -- 2 s typical, 13 s worst
//! measured (see `spawn_wifi` in the app crate) -- so the device looked dead
//! for the whole of it, and the only way to find out whether the radio was on
//! was to open the menu again.
//!
//! Drawn rather than loaded from `icons/`, deliberately. The glyph is the Mac
//! one -- concentric arcs over a dot -- and as SVG it would be four assets, one
//! per lit-arc count, that all have to reach `/mnt/us/plato/icons` before a
//! binary that references them; `just plato-binary` pushes the binary alone. A
//! shape this simple costs less as thirty lines of rasterizer than as a
//! deployment step that can be forgotten.

use crate::color::{Color, BLACK, WHITE};
use crate::context::Context;
use crate::font::Fonts;
use crate::framebuffer::{Framebuffer, UpdateMode};
use crate::geom::{surface_area, Rectangle};
use crate::gesture::GestureEvent;
use crate::view::{Bus, Event, Hub, Id, RenderData, RenderQueue, View, ID_FEEDER};
use std::f32::consts::FRAC_PI_2;

/// The unlit arcs. Light enough to read as absent, dark enough to keep the
/// glyph's outline -- which is what makes the lit ones legible as a *count*
/// rather than as a differently sized icon.
const DIM: Color = Color::Gray(186);

/// Half the fan's opening, from vertical. The Mac glyph is a little wider than
/// a right angle in total.
const HALF_SPAN: f32 = 0.85 * FRAC_PI_2 / 2.0;

/// Arc radii and the dot, as fractions of the glyph's height. Three arcs, so
/// three radii; the dot sits at the common center.
// Chosen so the gap between the dot and the first arc matches the gap between
// arcs -- at 0.42 the dot read as detached from the fan rather than part of it.
const RADII: [f32; 3] = [0.34, 0.64, 0.94];
const DOT_RADIUS: f32 = 0.11;
const STROKE: f32 = 0.135;

/// The glyph's height as a fraction of the slot it is given, matching the
/// optical weight of the battery beside it.
const GLYPH_SCALE: f32 = 0.46;

/// What the icon is saying. Derived from the context on every repaint rather
/// than stored, so it cannot drift from what the rest of the app believes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiState {
    /// The radio is off.
    Off,
    /// A transition is in flight -- either direction. The arcs animate.
    Connecting,
    /// The radio is on but nothing has confirmed a usable link yet.
    On,
    /// Associated and addressed.
    Online,
}

impl WifiState {
    pub fn from_context(context: &Context) -> WifiState {
        if context.wifi_busy {
            WifiState::Connecting
        } else if !context.settings.wifi {
            WifiState::Off
        } else if context.online {
            WifiState::Online
        } else {
            WifiState::On
        }
    }
}

pub struct Wifi {
    id: Id,
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    state: WifiState,
    /// Which arc the sweep has reached, counted from the dot outwards. Only
    /// meaningful while connecting.
    frame: usize,
}

impl Wifi {
    pub fn new(rect: Rectangle, context: &Context) -> Wifi {
        Wifi {
            id: ID_FEEDER.next(),
            rect,
            children: Vec::new(),
            state: WifiState::from_context(context),
            frame: 0,
        }
    }

    /// Re-read the state and repaint if anything actually moved.
    pub fn update(&mut self, rq: &mut RenderQueue, context: &Context) {
        let state = WifiState::from_context(context);
        match next_frame(self.state, state, self.frame) {
            None => (),
            Some(frame) => {
                self.state = state;
                self.frame = frame;
                // Fast, not Gui: a small region redrawn repeatedly is exactly
                // what the fast waveform is for. A Gui update here flashes.
                rq.add(RenderData::new(self.id, self.rect, UpdateMode::Fast));
            },
        }
    }

}

/// How many arcs are lit, outwards from the dot.
fn lit(state: WifiState, frame: usize) -> usize {
    match state {
        WifiState::Off => 0,
        WifiState::Connecting => frame,
        // On but unconfirmed: everything but the outermost arc, so the
        // difference from a settled link is visible without being alarming.
        WifiState::On => RADII.len() - 1,
        WifiState::Online => RADII.len(),
    }
}

/// What the next repaint should show, or `None` for "nothing changed, do not
/// touch the panel".
///
/// Pulled out of `update` so it can be tested without a `Context`, and because
/// the early return is the load-bearing part: this is consulted every 600 ms
/// for as long as a transition lasts, and an e-ink panel pays for an update
/// whether or not any pixel differs.
fn next_frame(current: WifiState, next: WifiState, frame: usize) -> Option<usize> {
    if next != WifiState::Connecting {
        // Settled: repaint once, on the way in, and then never again.
        return if next == current { None } else { Some(0) };
    }
    Some(if next == current {
        (frame + 1) % (RADII.len() + 1)
    } else {
        // Entering the transition: start the sweep at the dot, so it always
        // reads outwards.
        0
    })
}

/// Rasterize one arc of the fan: a circular stroke of `thickness`, centered on
/// `center` at `radius`, spanning `HALF_SPAN` either side of straight up, with
/// round caps.
///
/// Anti-aliased the same way `draw_disk` is -- signed distance to the boundary,
/// through `surface_area` -- because at this size the arcs are three pixels
/// thick and a hard edge reads as a staircase.
fn draw_arc(fb: &mut dyn Framebuffer, center: (f32, f32), radius: f32,
            thickness: f32, color: Color, clip: &Rectangle) {
    let half = thickness / 2.0;
    // The caps' centers, at either end of the arc's midline. Screen y grows
    // downwards, hence the negated cosine: the fan opens upwards.
    let caps = [
        (center.0 - radius * HALF_SPAN.sin(), center.1 - radius * HALF_SPAN.cos()),
        (center.0 + radius * HALF_SPAN.sin(), center.1 - radius * HALF_SPAN.cos()),
    ];

    for y in clip.min.y..clip.max.y {
        for x in clip.min.x..clip.max.x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let vx = px - center.0;
            let vy = py - center.1;
            let dist = (vx * vx + vy * vy).sqrt();

            // Angle away from straight up, signed; `vy` is negated so that the
            // upward direction is zero.
            let angle = vx.atan2(-vy);

            let (delta, gradient) = if angle.abs() <= HALF_SPAN {
                // Inside the wedge: the nearest boundary point is radially out.
                (dist - radius, vy.atan2(vx))
            } else {
                // Outside it: the nearest point is on whichever round cap is on
                // this side.
                let cap = if angle.is_sign_negative() { caps[0] } else { caps[1] };
                let cx = px - cap.0;
                let cy = py - cap.1;
                ((cx * cx + cy * cy).sqrt(), cy.atan2(cx))
            };

            let alpha = surface_area(delta.abs() - half, gradient);
            if alpha > 0.0 {
                fb.set_blended_pixel(x as u32, y as u32, color, alpha);
            }
        }
    }
}

impl View for Wifi {
    fn handle_event(&mut self, evt: &Event, _hub: &Hub, bus: &mut Bus, rq: &mut RenderQueue, context: &mut Context) -> bool {
        match *evt {
            Event::WifiTick => {
                self.update(rq, context);
                true
            },
            Event::Gesture(GestureEvent::Tap(center)) if self.rect.includes(center) => {
                // Straight to the toggle rather than to a menu: this icon
                // exists because the menu round trip was the problem. The app
                // ignores a request that arrives mid-transition, so a double
                // tap cannot get the radio into a confused state.
                bus.push_back(Event::SetWifi(!context.settings.wifi));
                true
            },
            _ => false,
        }
    }

    fn render(&self, fb: &mut dyn Framebuffer, _rect: Rectangle, _fonts: &mut Fonts) {
        fb.draw_rectangle(&self.rect, WHITE);

        // Every measure below is a fraction of the slot, which the top bar has
        // already sized for the panel -- so there is no scale_by_dpi here. The
        // constants Battery uses are in design units and need it; these are not.
        let height = self.rect.height() as f32 * GLYPH_SCALE;
        let thickness = (STROKE * height).max(2.0);

        // The dot is the fan's center, so the glyph hangs below the middle of
        // the slot by half its own height.
        let center = (self.rect.min.x as f32 + self.rect.width() as f32 / 2.0,
                      self.rect.min.y as f32 + (self.rect.height() as f32 + height) / 2.0);

        let lit = lit(self.state, self.frame);

        for (index, fraction) in RADII.iter().enumerate() {
            let color = if index < lit { BLACK } else { DIM };
            draw_arc(fb, center, fraction * height, thickness, color, &self.rect);
        }

        // The dot is always solid: it is the one part that says "there is a
        // radio here at all", and it doubles as the target the eye returns to
        // while the arcs sweep.
        fb.draw_disk(pt!(center.0 as i32, center.1 as i32),
                     (DOT_RADIUS * height).max(2.0) as i32,
                     BLACK);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::Pixmap;
    use crate::unit::scale_by_dpi;
    use crate::view::SMALL_BAR_HEIGHT;

    /// The slot the top bar gives this widget on the PW3: the small bar is
    /// square-slotted, so the side is the bar's height.
    fn slot() -> i32 {
        scale_by_dpi(SMALL_BAR_HEIGHT, 300) as i32
    }

    /// Every state at the size it will actually be drawn, written to
    /// `$TMPDIR`, so the glyph can be **looked at**. Same reasoning as the
    /// pairing view's dumper: one render settles what reading the code does
    /// not, and this one is a hand-rasterized shape at 55 px.
    ///
    /// ```text
    /// python3 xbuild.py host --test --package plato-core wifi::tests::dump_png -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn dump_png() {
        // `render` never touches the fonts, but its signature demands them, and
        // `Fonts::load` resolves its paths relative to the working directory.
        // Ignored by default partly for this: it changes the cwd out from under
        // any test that reads a fixture.
        std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).unwrap();
        let side = slot();
        // Four frames of the sweep, then the three settled states, laid out in
        // a row on one sheet -- comparing them side by side is the whole point.
        let states: Vec<(&str, WifiState, usize)> =
            vec![("spin0", WifiState::Connecting, 0),
                 ("spin1", WifiState::Connecting, 1),
                 ("spin2", WifiState::Connecting, 2),
                 ("spin3", WifiState::Connecting, 3),
                 ("off", WifiState::Off, 0),
                 ("on", WifiState::On, 0),
                 ("online", WifiState::Online, 0)];

        let mut fb = Pixmap::new((side * states.len() as i32) as u32, side as u32, 1);
        fb.draw_rectangle(&rect![0, 0, side * states.len() as i32, side], WHITE);

        for (index, (name, state, frame)) in states.iter().enumerate() {
            let x = index as i32 * side;
            let v = Wifi {
                id: 1,
                rect: rect![x, 0, x + side, side],
                children: Vec::new(),
                state: *state,
                frame: *frame,
            };
            v.render(&mut fb, v.rect, &mut crate::font::Fonts::load().unwrap());
            println!("{} at x={}", name, x);
        }

        let out = std::env::temp_dir().join("wifi-states.png");
        fb.save(out.to_str().unwrap()).unwrap();
        println!("{}", out.display());
    }

    /// The sweep starts at the dot, lights every arc in turn, and wraps -- a
    /// frame past the last arc would index nothing and read as a stall.
    #[test]
    fn the_sweep_lights_every_arc_in_turn_and_wraps() {
        let mut frame = next_frame(WifiState::Off, WifiState::Connecting, 7).unwrap();
        assert_eq!(frame, 0, "entering a transition restarts the sweep");

        let mut seen = Vec::new();
        for _ in 0..=RADII.len() {
            seen.push(lit(WifiState::Connecting, frame));
            frame = next_frame(WifiState::Connecting, WifiState::Connecting, frame).unwrap();
        }
        assert_eq!(seen, vec![0, 1, 2, 3]);
        assert_eq!(frame, 0, "the sweep wraps rather than running past RADII");
    }

    /// A settled radio must stop repainting. This is the check that keeps the
    /// panel from being driven every 600 ms for the rest of the session.
    #[test]
    fn a_settled_state_repaints_once_and_then_stops() {
        for state in [WifiState::Off, WifiState::On, WifiState::Online] {
            assert_eq!(next_frame(WifiState::Connecting, state, 2), Some(0),
                       "{state:?} must repaint on arrival");
            assert_eq!(next_frame(state, state, 0), None,
                       "{state:?} must not repaint while unchanged");
        }
    }

    /// The three settled states must be distinguishable, or the icon says
    /// nothing: "on but unconfirmed" is the one that would otherwise be
    /// indistinguishable from a working link.
    #[test]
    fn every_settled_state_lights_a_different_number_of_arcs() {
        let counts: Vec<usize> = [WifiState::Off, WifiState::On, WifiState::Online]
            .iter()
            .map(|state| lit(*state, 0))
            .collect();
        assert_eq!(counts, vec![0, RADII.len() - 1, RADII.len()]);
    }
}
