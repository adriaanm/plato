//! The pairing window on the e-ink: a code that must be readable across a room.
//!
//! This view holds no logic and owns nothing.  The window itself -- the
//! listener, the firewall rules, the deadline -- lives in the `plato` crate's
//! pairing thread, which reports here through [`Event::Pairing`].  Leaving the
//! view therefore does not end the window: it is bounded, single-use and closes
//! its own firewall rules whatever the screen is showing.
//!
//! Everything on screen exists because a rung of the discovery ladder needs it
//! (platokin `docs/pairing-candidates.md`): the code, the command to run, and
//! **the address** -- the zero-dependency last resort, because nothing can
//! filter a human reading a number.

use crate::color::{BLACK, WHITE};
use crate::context::Context;
use crate::device::CURRENT_DEVICE;
use crate::font::{font_from_style, Fonts, Style, FONT_SIZES, DISPLAY_FONT_SIZE,
                  DISPLAY_STYLE, NORMAL_STYLE, PAIRING_CODE_STYLE};
use crate::framebuffer::{Framebuffer, UpdateMode};
use crate::geom::{CornerSpec, Point, Rectangle};
use crate::unit::scale_by_dpi;
use crate::view::icon::Icon;
use crate::view::{Bus, Event, Hub, RenderData, RenderQueue, View};
use crate::view::{Id, ID_FEEDER};
use crate::view::SMALL_BAR_HEIGHT;

/// What the pairing thread reports back.  Exactly one terminal variant is sent
/// per window; everything else may repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    /// Seconds left in the window.
    Tick(u64),
    /// Someone got the code wrong.  The window deliberately stays open -- one
    /// wrong guess out of ~40 bits is a typo -- so this is not terminal.
    WrongCode { attempts: usize, max: usize },
    /// Terminal, and the only outcome that changed anything.
    Paired(String),
    /// Terminal: the window could not run, or was given up on.
    Failed(String),
    /// Terminal: nobody paired in time.
    Expired,
}

impl PairingStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, PairingStatus::Paired(..) | PairingStatus::Failed(..)
                     | PairingStatus::Expired)
    }

    /// One line, for when there is no pairing view to show it in.
    pub fn notification(&self) -> Option<String> {
        match self {
            PairingStatus::Paired(summary) => Some(summary.clone()),
            PairingStatus::Failed(reason) => Some(format!("Pairing failed: {}", reason)),
            PairingStatus::Expired => Some("The pairing window closed.".to_string()),
            _ => None,
        }
    }
}

pub struct Pairing {
    id: Id,
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    code: String,
    address: String,
    /// The last status shown, plus the remaining time when it is still running.
    status: PairingStatus,
    remaining: u64,
    finished: bool,
}

impl Pairing {
    pub fn new(rect: Rectangle, code: String, address: String, window_secs: u64,
               rq: &mut RenderQueue, _context: &mut Context) -> Pairing {
        let id = ID_FEEDER.next();
        let dpi = CURRENT_DEVICE.dpi;
        let small_height = scale_by_dpi(SMALL_BAR_HEIGHT, dpi) as i32;
        let dx = (rect.width() as i32 - small_height) / 2;
        let dy = rect.height() as i32 - 2 * small_height;
        let icon_rect = rect![rect.min.x + dx, rect.min.y + dy,
                              rect.min.x + dx + small_height, rect.min.y + dy + small_height];
        let icon = Icon::new("back", icon_rect, Event::Back)
                        .corners(Some(CornerSpec::Uniform(small_height / 2)));
        rq.add(RenderData::new(id, rect, UpdateMode::Full));
        Pairing {
            id,
            rect,
            children: vec![Box::new(icon) as Box<dyn View>],
            code,
            address,
            status: PairingStatus::Tick(window_secs),
            remaining: window_secs,
            finished: false,
        }
    }

    /// The code as it is shown: upper case, which reads cleaner on the panel
    /// (Adriaan, 2026-08-12).
    ///
    /// Presentation only, and safe: the canonical form is lower case -- it is
    /// what the SPAKE2 password bytes are derived from -- and `Code::parse`
    /// lowercases every character, so what is on screen round-trips.
    ///
    /// Done here rather than in `new` deliberately: a transformation applied in
    /// the constructor would be invisible to `dump_png`, which builds the
    /// struct by hand, and the renders would stop showing what ships.  That is
    /// exactly how the blank-screen bug hid.
    fn shown_code(&self) -> String {
        self.code.to_uppercase()
    }

    /// The command to type, with the code already in it.
    ///
    /// Adriaan, 2026-08-12: the screen should show the command *including*
    /// `--code`, so it can be typed straight across rather than read, held in
    /// the head, and typed at a prompt that then asks for the code again.  The
    /// code is still shown on its own line above, big, because that is the part
    /// people check a character at a time -- and it matches the case shown
    /// there, because two spellings of one code invites the question of which
    /// one is meant.
    fn command(&self) -> String {
        format!("platonic --pair --code {}", self.shown_code())
    }

    /// The closing lines: the address as the zero-dependency fallback, and how
    /// long is left.
    fn footer(&self) -> Vec<String> {
        let mut lines = vec![format!("This reader: {}", self.address),
                             format!("Closes in {}", clock(self.remaining))];
        if let PairingStatus::WrongCode { attempts, max } = self.status {
            lines.push(format!("Wrong code, try again ({} of {}).", attempts, max));
        }
        lines
    }

    /// What replaces the whole code-and-command block once the window is over.
    fn outcome(&self) -> Vec<String> {
        match &self.status {
            PairingStatus::Paired(summary) => vec![summary.clone(), "Tap Back.".to_string()],
            PairingStatus::Failed(reason) => vec!["Pairing failed.".to_string(), reason.clone()],
            PairingStatus::Expired => vec!["The pairing window closed.".to_string(),
                                           "Nothing was changed.".to_string()],
            _ => Vec::new(),
        }
    }
}

/// Draw one centred line, grown or shrunk to `target` px wide.
///
/// Everything on this screen is sized to the panel rather than given a guessed
/// point size: it is read across a room, and a layout that merely happens to
/// fit at 300 dpi is a layout that will not fit on the next panel.  `cap` stops
/// a short string (a two-word heading) from being blown up to absurdity.
fn draw_fitted(fb: &mut dyn Framebuffer, fonts: &mut Fonts, style: &Style, dpi: u16,
               text: &str, origin: Point, width: i32, dy: i32,
               target: i32, cap: u32) -> i32 {
    let font = font_from_style(fonts, style, dpi);
    // Always re-assert the size: these fonts are shared and cached, so a
    // previous draw's set_size is still in effect.
    font.set_size(style.size, dpi);
    let mut plan = font.plan(text, None, None);
    if plan.width > 0 && target > 0 {
        let size = ((style.size as f32 * target as f32 / plan.width as f32) as u32).min(cap);
        font.set_size(size, dpi);
        plan = font.plan(text, None, None);
    }
    font.render(fb, BLACK, &plan, origin + pt!((width - plan.width) / 2, dy));
    font.line_height()
}

fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl View for Pairing {
    fn handle_event(&mut self, evt: &Event, _hub: &Hub, _bus: &mut Bus, rq: &mut RenderQueue,
                    _context: &mut Context) -> bool {
        match evt {
            Event::Pairing(status) => {
                // A terminal status is final: a Tick still in flight when the
                // window ended must not overwrite the outcome.
                if self.finished {
                    return true;
                }
                self.finished = status.is_terminal();
                if let PairingStatus::Tick(remaining) = status {
                    self.remaining = *remaining;
                    // A tick under a running window repaints the time only;
                    // keep whatever non-terminal message is showing.
                    if matches!(self.status, PairingStatus::WrongCode { .. }) {
                        rq.add(RenderData::new(self.id, self.rect, UpdateMode::Gui));
                        return true;
                    }
                }
                self.status = status.clone();
                // A full update for the outcome (it is the thing being read),
                // a partial one for the countdown.
                let mode = if self.finished { UpdateMode::Full } else { UpdateMode::Gui };
                rq.add(RenderData::new(self.id, self.rect, mode));
                true
            },
            _ => false,
        }
    }

    fn render(&self, fb: &mut dyn Framebuffer, _rect: Rectangle, fonts: &mut Fonts) {
        let dpi = CURRENT_DEVICE.dpi;
        let width = self.rect.width() as i32;
        let height = self.rect.height() as i32;

        fb.draw_rectangle(&self.rect, WHITE);
        let origin = self.rect.min;

        // The panel is the unit of layout: this screen is read across a room
        // and typed from, so everything is placed as a fraction of the height
        // and grown to a fraction of the width.  Nothing here is a point size
        // that happens to look right at 300 dpi.
        let at = |f: f32| (height as f32 * f) as i32;

        draw_fitted(fb, fonts, &DISPLAY_STYLE, dpi, "Pair a Mac", origin, width,
                    at(0.12), width / 2, DISPLAY_FONT_SIZE);

        if self.status.is_terminal() {
            // The code goes the moment the window is over: a code still on
            // screen reads as a window still open, which is exactly what it is
            // not.  The outcome takes its place, in its size.
            let mut dy = at(0.40);
            for line in self.outcome() {
                let h = draw_fitted(fb, fonts, &NORMAL_STYLE, dpi, &line, origin,
                                    width, dy, (width * 3) / 4, FONT_SIZES[2]);
                dy += 2 * h;
            }
            return;
        }

        // The code is the whole point: it is what somebody checks a character
        // at a time, so it gets the panel's full width and no cap.
        draw_fitted(fb, fonts, &PAIRING_CODE_STYLE, dpi, &self.shown_code(), origin,
                    width, at(0.32), (width * 6) / 7, u32::MAX);

        // The command, with the code already in it, so it can be typed
        // straight across instead of being memorised (Adriaan, 2026-08-12).
        // Monospace and nearly full width: this is the line that gets
        // transcribed, so the WIDTH is what decides its size -- the cap is
        // deliberately loose enough not to bind, or the one line somebody has
        // to read while typing ends up the smallest thing on the screen.
        draw_fitted(fb, fonts, &NORMAL_STYLE, dpi, "On your Mac, run:", origin,
                    width, at(0.48), width / 3, FONT_SIZES[1]);
        draw_fitted(fb, fonts, &PAIRING_CODE_STYLE, dpi, &self.command(), origin,
                    width, at(0.57), (width * 9) / 10, 4 * FONT_SIZES[2]);

        let mut dy = at(0.72);
        for line in self.footer() {
            let h = draw_fitted(fb, fonts, &NORMAL_STYLE, dpi, &line, origin,
                                width, dy, width / 2, FONT_SIZES[1]);
            dy += 2 * h;
        }
    }

    /// **Load-bearing, and it does not look it.** `process_render_queue` calls
    /// a view's own `render` only when `view.len() == 0 || view.is_background()`
    /// -- a view that has children is otherwise taken to be a mere container,
    /// and only its children get drawn.  This view has one child (the Back
    /// icon) and draws everything that matters itself, so without this the
    /// code, the address and the countdown are never drawn at all.
    ///
    /// The failure is silent and misleading: the queued Full update still runs,
    /// so the panel **flashes and then shows nothing new**, which reads as "the
    /// window never opened" rather than "the view drew nothing" -- the pairing
    /// thread was armed and listening the whole time.  Confirmed on the device
    /// 2026-08-12.  `dump_png` missed it because it builds this struct by hand
    /// with no children and calls `render` directly, exercising a shape `new`
    /// never produces.
    fn is_background(&self) -> bool {
        true
    }

    fn might_rotate(&self) -> bool {
        false
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

    /// Render every state at the PW3's real geometry and write the PNGs to
    /// `$TMPDIR`, so the layout can be **looked at** rather than reasoned
    /// about -- the lesson of 2026-08-11, where reading the code produced a
    /// confident wrong claim and one render settled it.
    ///
    /// ```text
    /// python3 xbuild.py host --test --package plato-core dump_png -- --ignored
    /// ```
    ///
    /// Ignored by default for two reasons: it writes files, and it changes the
    /// process's working directory (the font paths are relative to the repo
    /// root), which would race any other test that reads a fixture.
    #[test]
    #[ignore]
    fn dump_png() {
        use crate::framebuffer::Pixmap;
        use crate::font::Fonts;
        std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).unwrap();
        let mut fb = Pixmap::new(1072, 1448, 1);
        let mut fonts = Fonts::load().unwrap();
        for (name, status) in [("armed", PairingStatus::Tick(165)),
                               ("wrong", PairingStatus::WrongCode { attempts: 1, max: 10 }),
                               ("paired", PairingStatus::Paired("Paired. 3 keys authorized.".into())),
                               ("expired", PairingStatus::Expired)] {
            let v = Pairing {
                id: 1,
                rect: rect![0, 0, 1072, 1448],
                children: Vec::new(),
                code: "abcd-2345".to_string(),
                address: "192.168.178.190:30305".to_string(),
                status,
                remaining: 165,
                finished: false,
            };
            v.render(&mut fb, v.rect, &mut fonts);
            let out = std::env::temp_dir().join(format!("pairing-{}.png", name));
            fb.save(out.to_str().unwrap()).unwrap();
            println!("{}", out.display());
        }
    }

    /// The bug that shipped to the device on 2026-08-12, as a test.
    ///
    /// `process_render_queue` draws a view's own content only when
    /// `len() == 0 || is_background()`.  This view has a child *and* draws
    /// everything that matters itself, so if that ever stops holding the panel
    /// flashes and shows nothing -- with the pairing thread armed and
    /// listening behind it, which is what made it read as a UI that never
    /// opened.
    ///
    /// `dump_png` could not catch it: it hand-builds the struct with no
    /// children and calls `render` directly, so it exercises a shape `new`
    /// never produces and never goes near the queue's decision.  This asserts
    /// the invariant on the children `new` actually installs.
    #[test]
    fn a_view_that_draws_itself_must_be_rendered_by_the_queue() {
        let icon = Icon::new("back", rect![0, 0, 10, 10], Event::Back);
        let v = Pairing {
            id: 1,
            rect: rect![0, 0, 1072, 1448],
            children: vec![Box::new(icon) as Box<dyn View>],
            code: "abcd-2345".to_string(),
            address: "192.168.178.190:30305".to_string(),
            status: PairingStatus::Tick(165),
            remaining: 165,
            finished: false,
        };
        assert!(!v.children.is_empty(),
                "this test is pointless if the view has no children");
        assert!(v.len() == 0 || v.is_background(),
                "Pairing draws its own content but the render queue would skip \
                 it: the panel flashes and stays blank");
    }

    #[test]
    fn remaining_time_reads_as_a_clock() {
        assert_eq!(clock(180), "3:00");
        assert_eq!(clock(59), "0:59");
        assert_eq!(clock(0), "0:00");
    }

    #[test]
    fn only_the_outcomes_are_terminal() {
        assert!(!PairingStatus::Tick(10).is_terminal());
        assert!(!PairingStatus::WrongCode { attempts: 1, max: 10 }.is_terminal());
        assert!(PairingStatus::Paired("x".into()).is_terminal());
        assert!(PairingStatus::Failed("x".into()).is_terminal());
        assert!(PairingStatus::Expired.is_terminal());
    }
}
