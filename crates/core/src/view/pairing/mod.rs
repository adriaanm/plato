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
use crate::font::{font_from_style, Fonts, NORMAL_STYLE, PAIRING_CODE_STYLE};
use crate::framebuffer::{Framebuffer, UpdateMode};
use crate::geom::{CornerSpec, Rectangle};
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

    /// The line under the code: what to do, or what happened.
    fn message(&self) -> Vec<String> {
        match &self.status {
            PairingStatus::Tick(..) | PairingStatus::WrongCode { .. } => {
                let mut lines = vec![
                    "Run: platonic --pair".to_string(),
                    format!("This reader: {}", self.address),
                    format!("Closes in {}", clock(self.remaining)),
                ];
                if let PairingStatus::WrongCode { attempts, max } = self.status {
                    lines.push(format!("Wrong code, try again ({} of {}).", attempts, max));
                }
                lines
            },
            PairingStatus::Paired(summary) => vec![summary.clone(), "Tap Back.".to_string()],
            PairingStatus::Failed(reason) => vec!["Pairing failed.".to_string(), reason.clone()],
            PairingStatus::Expired => vec!["The pairing window closed.".to_string(),
                                           "Nothing was changed.".to_string()],
        }
    }
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

        let font = font_from_style(fonts, &NORMAL_STYLE, dpi);
        let plan = font.plan("Pair a Mac", None, None);
        let mut dy = height / 6;
        font.render(fb, BLACK, &plan, self.rect.min + pt!((width - plan.width) / 2, dy));

        // The code is the whole point, so it is grown to the panel rather than
        // given a guessed size: measure, scale to the target width, measure
        // again.  It is never clipped and never ellipsized.
        //
        // It disappears the moment the window is over: a code still on screen
        // reads as a window still open, which is exactly what it is not.
        dy += height / 6;
        if !self.status.is_terminal() {
            let font = font_from_style(fonts, &PAIRING_CODE_STYLE, dpi);
            let target = (width * 4) / 5;
            let mut plan = font.plan(&self.code, None, None);
            if plan.width != target && plan.width > 0 {
                let size = (PAIRING_CODE_STYLE.size as f32 * target as f32
                            / plan.width as f32) as u32;
                font.set_size(size, dpi);
                plan = font.plan(&self.code, None, None);
            }
            font.render(fb, BLACK, &plan, self.rect.min + pt!((width - plan.width) / 2, dy));
            dy += height / 8;
        }
        let font = font_from_style(fonts, &NORMAL_STYLE, dpi);
        let step = 2 * font.line_height();
        for line in self.message() {
            let plan = font.plan(line, None, None);
            font.render(fb, BLACK, &plan, self.rect.min + pt!((width - plan.width) / 2, dy));
            dy += step;
        }
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
