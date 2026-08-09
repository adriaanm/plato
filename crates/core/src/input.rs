use std::mem::{self, MaybeUninit};
use std::ptr;
use std::slice;
use std::thread;
use std::io::Read;
use std::fs::File;
use std::sync::mpsc::{self, Sender, Receiver};
use std::os::unix::io::AsRawFd;
use std::ffi::CString;
use fxhash::FxHashMap;
use crate::framebuffer::Display;
use crate::settings::ButtonScheme;
use crate::device::CURRENT_DEVICE;
use crate::geom::{Point, LinearDir};
use anyhow::{Error, Context};

// Event types
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;

// Event codes
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const MSC_RAW: u16 = 0x03;
pub const SYN_REPORT: u16 = 0x00;

// Event values
pub const MSC_RAW_GSENSOR_PORTRAIT_DOWN: i32 = 0x17;
pub const MSC_RAW_GSENSOR_PORTRAIT_UP: i32 = 0x18;
pub const MSC_RAW_GSENSOR_LANDSCAPE_RIGHT: i32 = 0x19;
pub const MSC_RAW_GSENSOR_LANDSCAPE_LEFT: i32 = 0x1a;
// pub const MSC_RAW_GSENSOR_BACK: i32 = 0x1b;
// pub const MSC_RAW_GSENSOR_FRONT: i32 = 0x1c;

// The indices of this clockwise ordering of the sensor values match the Forma's rotation values.
pub const GYROSCOPE_ROTATIONS: [i32; 4] = [MSC_RAW_GSENSOR_LANDSCAPE_LEFT, MSC_RAW_GSENSOR_PORTRAIT_UP,
                                           MSC_RAW_GSENSOR_LANDSCAPE_RIGHT, MSC_RAW_GSENSOR_PORTRAIT_DOWN];

pub const VAL_RELEASE: i32 = 0;
pub const VAL_PRESS: i32 = 1;
pub const VAL_REPEAT: i32 = 2;

// Key codes
pub const KEY_POWER: u16 = 116;
pub const KEY_HOME: u16 = 102;
pub const KEY_LIGHT: u16 = 90;
pub const KEY_BACKWARD: u16 = 193;
pub const KEY_FORWARD: u16 = 194;
pub const PEN_ERASE: u16 = 331;
pub const PEN_HIGHLIGHT: u16 = 332;
pub const SLEEP_COVER: [u16; 2] = [59, 35];
// Synthetic touch button
pub const BTN_TOUCH: u16 = 330;
// Tool-type keys a multi-touch driver emits alongside BTN_TOUCH. The PW3's
// `cyttsp4_mt` sends both (Confirmed in a live capture); they carry no
// information the slot state machine doesn't already have, and letting them
// through would surface as `ButtonCode::Raw` presses on every tap. Ignored for
// `MultiSlot` only, so no Kobo's button stream changes.
pub const BTN_TOOL_FINGER: u16 = 325;
pub const BTN_TOOL_DOUBLETAP: u16 = 333;
pub const TOUCH_TOOL_KEYS: [u16; 2] = [BTN_TOOL_FINGER, BTN_TOOL_DOUBLETAP];
// The following key codes are fake, and are used to support
// software toggles within this design
pub const KEY_ROTATE_DISPLAY: u16 = 0xffff;
pub const KEY_BUTTON_SCHEME: u16 = 0xfffe;

pub const SINGLE_TOUCH_CODES: TouchCodes = TouchCodes {
    pressure: ABS_PRESSURE,
    x: ABS_X,
    y: ABS_Y,
};

pub const MULTI_TOUCH_CODES_A: TouchCodes = TouchCodes {
    pressure: ABS_MT_TOUCH_MAJOR,
    x: ABS_MT_POSITION_X,
    y: ABS_MT_POSITION_Y,
};

pub const MULTI_TOUCH_CODES_B: TouchCodes = TouchCodes {
    pressure: ABS_MT_PRESSURE,
    .. MULTI_TOUCH_CODES_A
};

/// Slot-tracked protocol B: `ABS_MT_POSITION_X`/`_Y` only.
///
/// `pressure` has to be *some* code because `TouchCodes` has the field, and
/// `ABS_MT_TOUCH_MAJOR` is the harmless choice — but the slot state machine
/// never reads it. On a panel like the PW3's `cyttsp4_mt`, no pressure axis is
/// advertised at all, so a pressure code that never arrives is exactly right.
pub const MULTI_TOUCH_CODES_SLOT: TouchCodes = TouchCodes {
    pressure: ABS_MT_TOUCH_MAJOR,
    .. MULTI_TOUCH_CODES_A
};

#[repr(C)]
pub struct InputEvent {
    pub time: libc::timeval,
    pub kind: u16, // type
    pub code: u16,
    pub value: i32,
}

// Handle different touch protocols
#[derive(Debug)]
pub struct TouchCodes {
    pressure: u16,
    x: u16,
    y: u16,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TouchProto {
    Single,
    MultiA,
    MultiB, // Pressure won't indicate a finger release.
    MultiC,
    /// Protocol B tracked by **slot**, with no pressure axis anywhere.
    ///
    /// `MultiA`/`MultiB`/`MultiC` all key a contact on a pressure-ish axis and
    /// ignore `ABS_MT_SLOT` entirely, which works only on drivers that re-send
    /// the whole contact state every packet. This variant implements the
    /// kernel's protocol B as documented
    /// (`Documentation/input/multi-touch-protocol.txt`): the driver keeps slot
    /// state and sends **only what changed**, `ABS_MT_SLOT` selects the slot,
    /// and `ABS_MT_TRACKING_ID` alone opens (`>= 0`) and closes (`-1`) a
    /// contact. Finger identity is the slot's current tracking id.
    ///
    /// Confirmed necessary on the Kindle PW3 (`cyttsp4_mt`, `/dev/input/event1`):
    /// its ABS bitmap is exactly SLOT + POSITION_X + POSITION_Y + TRACKING_ID —
    /// no `ABS_MT_PRESSURE`, no plain `ABS_X`/`ABS_Y` — so every pressure-keyed
    /// path above would see the panel as permanently untouched.
    MultiSlot,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FingerStatus {
    Down,
    Motion,
    Up,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ButtonStatus {
    Pressed,
    Released,
    Repeated,
}

impl ButtonStatus {
    pub fn try_from_raw(value: i32) -> Option<ButtonStatus> {
        match value {
            VAL_RELEASE => Some(ButtonStatus::Released),
            VAL_PRESS => Some(ButtonStatus::Pressed),
            VAL_REPEAT => Some(ButtonStatus::Repeated),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ButtonCode {
    Power,
    Home,
    Light,
    Backward,
    Forward,
    Erase,
    Highlight,
    Raw(u16),
}

impl ButtonCode {
    fn from_raw(code: u16, rotation: i8, button_scheme: ButtonScheme) -> ButtonCode {
        match code {
            KEY_POWER => ButtonCode::Power,
            KEY_HOME => ButtonCode::Home,
            KEY_LIGHT => ButtonCode::Light,
            KEY_BACKWARD => resolve_button_direction(LinearDir::Backward, rotation, button_scheme),
            KEY_FORWARD => resolve_button_direction(LinearDir::Forward, rotation, button_scheme),
            PEN_ERASE => ButtonCode::Erase,
            PEN_HIGHLIGHT => ButtonCode::Highlight,
            _ => ButtonCode::Raw(code)
        }
    }
}

fn resolve_button_direction(mut direction: LinearDir, rotation: i8, button_scheme: ButtonScheme) -> ButtonCode {
    if (CURRENT_DEVICE.should_invert_buttons(rotation)) ^ (button_scheme == ButtonScheme::Inverted) {
        direction = direction.opposite();
    }

    if direction == LinearDir::Forward {
        return ButtonCode::Forward;
    }

    ButtonCode::Backward
}

pub fn display_rotate_event(n: i8) -> InputEvent {
    let mut tp = libc::timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { libc::gettimeofday(&mut tp, ptr::null_mut()); }
    InputEvent {
        time: tp,
        kind: EV_KEY,
        code: KEY_ROTATE_DISPLAY,
        value: n as i32,
    }
}

pub fn button_scheme_event(v: i32) -> InputEvent {
    let mut tp = libc::timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { libc::gettimeofday(&mut tp, ptr::null_mut()); }
    InputEvent {
        time: tp,
        kind: EV_KEY,
        code: KEY_BUTTON_SCHEME,
        value: v,
    }
}

#[derive(Debug, Copy, Clone)]
pub enum DeviceEvent {
    Finger {
        id: i32,
        time: f64,
        status: FingerStatus,
        position: Point,
    },
    Button {
        time: f64,
        code: ButtonCode,
        status: ButtonStatus,
    },
    Plug(PowerSource),
    Unplug(PowerSource),
    RotateScreen(i8),
    CoverOn,
    CoverOff,
    NetUp,
    UserActivity,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PowerSource {
    Host,
    Wall,
}

pub fn seconds(time: libc::timeval) -> f64 {
    time.tv_sec as f64 + time.tv_usec as f64 / 1e6
}

pub fn raw_events(paths: Vec<String>) -> (Sender<InputEvent>, Receiver<InputEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    thread::spawn(move || parse_raw_events(&paths, &tx));
    (tx2, rx)
}

pub fn parse_raw_events(paths: &[String], tx: &Sender<InputEvent>) -> Result<(), Error> {
    let mut files = Vec::new();
    let mut pfds = Vec::new();

    for path in paths.iter() {
        let file = File::open(path)
                        .with_context(|| format!("can't open input file {}", path))?;
        let fd = file.as_raw_fd();
        files.push(file);
        pfds.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    }

    loop {
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            break;
        }
        for (pfd, mut file) in pfds.iter().zip(&files) {
            if pfd.revents & libc::POLLIN != 0 {
                let mut input_event = MaybeUninit::<InputEvent>::uninit();
                unsafe {
                    let event_slice = slice::from_raw_parts_mut(input_event.as_mut_ptr() as *mut u8,
                                                                mem::size_of::<InputEvent>());
                    if file.read_exact(event_slice).is_err() {
                        break;
                    }
                    tx.send(input_event.assume_init()).ok();
                }
            }
        }
    }

    Ok(())
}

pub fn usb_events() -> Receiver<DeviceEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || parse_usb_events(&tx));
    rx
}

fn parse_usb_events(tx: &Sender<DeviceEvent>) {
    let path = CString::new("/tmp/nickel-hardware-status").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_NONBLOCK | libc::O_RDWR) };

    if fd < 0 {
        return;
    }

    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    const BUF_LEN: usize = 256;

    loop {
        let ret = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, -1) };

        if ret < 0 {
            break;
        }

        let buf = CString::new(vec![1; BUF_LEN]).unwrap();
        let c_buf = buf.into_raw();

        if pfd.revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(fd, c_buf as *mut libc::c_void, BUF_LEN as libc::size_t) };
            let buf = unsafe { CString::from_raw(c_buf) };
            if n > 0 {
                if let Ok(s) = buf.to_str() {
                    for msg in s[..n as usize].lines() {
                        if msg == "usb plug add" {
                            tx.send(DeviceEvent::Plug(PowerSource::Host)).ok();
                        } else if msg == "usb plug remove" {
                            tx.send(DeviceEvent::Unplug(PowerSource::Host)).ok();
                        } else if msg == "usb ac add" {
                            tx.send(DeviceEvent::Plug(PowerSource::Wall)).ok();
                        } else if msg == "usb ac remove" {
                            tx.send(DeviceEvent::Unplug(PowerSource::Wall)).ok();
                        } else if msg.starts_with("network bound") {
                            tx.send(DeviceEvent::NetUp).ok();
                        }
                    }
                }
            } else {
                break;
            }
        }
    }
}

pub fn device_events(rx: Receiver<InputEvent>, display: Display, button_scheme: ButtonScheme) -> Receiver<DeviceEvent> {
    let (ty, ry) = mpsc::channel();
    thread::spawn(move || parse_device_events(&rx, &ty, display, button_scheme));
    ry
}

struct TouchState {
    position: Point,
    pressure: i32,
}

impl Default for TouchState {
    fn default() -> Self {
        TouchState {
            position: Point::default(),
            pressure: 0,
        }
    }
}

/// One driver-maintained contact slot.
#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    /// The identity of the contact currently in this slot, `None` between
    /// contacts. This is what the gesture layer sees as the finger id.
    tracking_id: Option<i32>,
    /// The slot's position, which **persists across `SYN_REPORT`**: protocol B
    /// drivers only re-send an axis when it moves.
    position: Point,
    /// The position last reported to the gesture layer, i.e. `Some` exactly
    /// when a `FingerStatus::Down` has already been emitted for `tracking_id`.
    reported: Option<Point>,
    /// A contact that ended and still owes a `FingerStatus::Up` at the next
    /// `SYN_REPORT`: its id, its last position, and whether it was ever
    /// reported down.
    lifted: Option<(i32, Point, bool)>,
}

impl Slot {
    /// End the current contact, if any, queueing its `Up` for the next sync.
    fn lift(&mut self) {
        if let Some(id) = self.tracking_id.take() {
            self.lifted = Some((id, self.position, self.reported.is_some()));
            self.reported = None;
        }
    }
}

/// A protocol-B slot tracker: `TouchProto::MultiSlot`'s whole implementation.
///
/// Split out of `parse_device_events` so it can be driven by a synthetic event
/// stream in tests — the device it was written for holds `EVIOCGRAB` on its
/// touch node whenever KOReader runs, so a live capture is expensive and a
/// unit test is free.
pub struct MultiSlotTracker {
    slots: Vec<Slot>,
    current: usize,
}

/// Slots above this are ignored rather than allocated: a stray `ABS_MT_SLOT`
/// value must not be able to allocate memory. `cyttsp4` reports 10 at most.
const MAX_SLOTS: usize = 32;

impl Default for MultiSlotTracker {
    fn default() -> Self {
        MultiSlotTracker { slots: Vec::new(), current: 0 }
    }
}

impl MultiSlotTracker {
    pub fn new() -> MultiSlotTracker {
        MultiSlotTracker::default()
    }

    fn current_slot(&mut self) -> Option<&mut Slot> {
        if self.current >= MAX_SLOTS {
            return None;
        }
        if self.current >= self.slots.len() {
            self.slots.resize(self.current + 1, Slot::default());
        }
        self.slots.get_mut(self.current)
    }

    /// Fold one `EV_ABS` event into the slot state. Emits nothing: protocol B
    /// packets are only meaningful at their `SYN_REPORT`.
    pub fn handle_abs(&mut self, code: u16, value: i32, tc: &TouchCodes,
                      mirror_x: bool, mirror_y: bool, dims: (u32, u32)) {
        if code == ABS_MT_SLOT {
            if value >= 0 {
                self.current = value as usize;
            }
            return;
        }

        let Some(slot) = self.current_slot() else { return };

        if code == ABS_MT_TRACKING_ID {
            if value >= 0 {
                // A driver may re-send the id every packet, or never again
                // after the contact begins. Both mean "the same contact".
                if slot.tracking_id != Some(value) {
                    // A new id without an intervening -1 is an implicit lift.
                    slot.lift();
                    slot.tracking_id = Some(value);
                }
            } else {
                slot.lift();
            }
        } else if code == tc.x {
            slot.position.x = if mirror_x {
                dims.0 as i32 - 1 - value
            } else {
                value
            };
        } else if code == tc.y {
            slot.position.y = if mirror_y {
                dims.1 as i32 - 1 - value
            } else {
                value
            };
        }
    }

    /// Emit the frame's finger events, in slot order: every pending `Up`
    /// first (matching the `MultiB` path, which sweeps releases before
    /// presses), then every `Down`/`Motion`.
    pub fn sync(&mut self, time: f64, ty: &Sender<DeviceEvent>) {
        for slot in &mut self.slots {
            if let Some((id, position, reported)) = slot.lifted.take() {
                // A contact that began and ended inside one frame still has to
                // look like a tap to the gesture layer.
                if !reported {
                    ty.send(DeviceEvent::Finger { id, time, status: FingerStatus::Down, position }).ok();
                }
                ty.send(DeviceEvent::Finger { id, time, status: FingerStatus::Up, position }).ok();
            }
        }

        for slot in &mut self.slots {
            let Some(id) = slot.tracking_id else { continue };
            match slot.reported {
                None => {
                    ty.send(DeviceEvent::Finger { id, time, status: FingerStatus::Down,
                                                  position: slot.position }).ok();
                    slot.reported = Some(slot.position);
                },
                Some(previous) if previous != slot.position => {
                    ty.send(DeviceEvent::Finger { id, time, status: FingerStatus::Motion,
                                                  position: slot.position }).ok();
                    slot.reported = Some(slot.position);
                },
                _ => (),
            }
        }
    }
}

pub fn parse_device_events(rx: &Receiver<InputEvent>, ty: &Sender<DeviceEvent>, display: Display, button_scheme: ButtonScheme) {
    let mut id = 0;
    let mut last_activity = -60;
    let Display { mut dims, mut rotation } = display;
    let mut fingers: FxHashMap<i32, Point> = FxHashMap::default();
    let mut packets: FxHashMap<i32, TouchState> = FxHashMap::default();
    let proto = CURRENT_DEVICE.proto;

    let mut tc = match proto {
        TouchProto::Single => SINGLE_TOUCH_CODES,
        TouchProto::MultiA => MULTI_TOUCH_CODES_A,
        TouchProto::MultiB => MULTI_TOUCH_CODES_B,
        TouchProto::MultiC => MULTI_TOUCH_CODES_B,
        TouchProto::MultiSlot => MULTI_TOUCH_CODES_SLOT,
    };

    // `Some` only for `MultiSlot`, so every other protocol runs exactly the
    // code it ran before this variant existed.
    let mut slots = (proto == TouchProto::MultiSlot).then(MultiSlotTracker::new);

    if proto == TouchProto::Single {
        packets.insert(id, TouchState::default());
    }

    let (mut mirror_x, mut mirror_y) = CURRENT_DEVICE.should_mirror_axes(rotation);
    if CURRENT_DEVICE.should_swap_axes(rotation) {
        mem::swap(&mut tc.x, &mut tc.y);
    }

    let mut button_scheme = button_scheme;

    while let Ok(evt) = rx.recv() {
        if evt.kind == EV_ABS {
            if let Some(slots) = slots.as_mut() {
                slots.handle_abs(evt.code, evt.value, &tc, mirror_x, mirror_y, dims);
            } else if evt.code == ABS_MT_TRACKING_ID {
                if evt.value >= 0 {
                    id = evt.value;
                    packets.insert(id, TouchState::default());
                }
            } else if evt.code == tc.x {
                if let Some(state) = packets.get_mut(&id) {
                    state.position.x = if mirror_x {
                        dims.0 as i32 - 1 - evt.value
                    } else {
                        evt.value
                    };
                }
            } else if evt.code == tc.y {
                if let Some(state) = packets.get_mut(&id) {
                    state.position.y = if mirror_y {
                        dims.1 as i32 - 1 - evt.value
                    } else {
                        evt.value
                    };
                }
            } else if evt.code == tc.pressure {
                if let Some(state) = packets.get_mut(&id) {
                    state.pressure = evt.value;
                    if proto == TouchProto::Single && CURRENT_DEVICE.mark() == 3 && state.pressure == 0 {
                        state.position.x = dims.0 as i32 - 1 - state.position.x;
                        mem::swap(&mut state.position.x, &mut state.position.y);
                    }
                }
            }
        } else if evt.kind == EV_SYN && evt.code == SYN_REPORT {
            // The absolute value accounts for the wrapping around that might occur,
            // since `tv_sec` can't grow forever.
            if (evt.time.tv_sec - last_activity).abs() >= 60 {
                last_activity = evt.time.tv_sec;
                ty.send(DeviceEvent::UserActivity).ok();
            }

            if let Some(slots) = slots.as_mut() {
                slots.sync(seconds(evt.time), ty);
                continue;
            }

            if proto == TouchProto::MultiB {
                fingers.retain(|other_id, other_position| {
                    packets.contains_key(&other_id) ||
                    ty.send(DeviceEvent::Finger {
                        id: *other_id,
                        time: seconds(evt.time),
                        status: FingerStatus::Up,
                        position: *other_position,
                    }).is_err()
                });
            }

            for (&id, state) in &packets {
                if let Some(&pos) = fingers.get(&id) {
                    if state.pressure > 0 {
                        if state.position != pos {
                            ty.send(DeviceEvent::Finger {
                                id,
                                time: seconds(evt.time),
                                status: FingerStatus::Motion,
                                position: state.position,
                            }).unwrap();
                            fingers.insert(id, state.position);
                        }
                    } else {
                        ty.send(DeviceEvent::Finger {
                            id,
                            time: seconds(evt.time),
                            status: FingerStatus::Up,
                            position: state.position,
                        }).unwrap();
                        fingers.remove(&id);
                    }
                } else if state.pressure > 0 {
                    ty.send(DeviceEvent::Finger {
                        id,
                        time: seconds(evt.time),
                        status: FingerStatus::Down,
                        position: state.position,
                    }).unwrap();
                    fingers.insert(id, state.position);
                }
            }

            if proto != TouchProto::Single {
                packets.clear();
            }
        } else if evt.kind == EV_KEY {
            if SLEEP_COVER.contains(&evt.code) {
                if evt.value == VAL_PRESS {
                    ty.send(DeviceEvent::CoverOn).ok();
                } else if evt.value == VAL_RELEASE {
                    ty.send(DeviceEvent::CoverOff).ok();
                } else if evt.value == VAL_REPEAT {
                    ty.send(DeviceEvent::CoverOn).ok();
                }
            } else if evt.code == KEY_BUTTON_SCHEME {
                if evt.value == VAL_PRESS {
                    button_scheme = ButtonScheme::Inverted;
                } else {
                    button_scheme = ButtonScheme::Natural;
                }
            } else if evt.code == KEY_ROTATE_DISPLAY {
                let next_rotation = evt.value as i8;
                if next_rotation != rotation {
                    let delta = (rotation - next_rotation).abs();
                    if delta % 2 == 1 {
                        mem::swap(&mut tc.x, &mut tc.y);
                        mem::swap(&mut dims.0, &mut dims.1);
                    }
                    rotation = next_rotation;
                    let should_mirror = CURRENT_DEVICE.should_mirror_axes(rotation);
                    mirror_x = should_mirror.0;
                    mirror_y = should_mirror.1;
                }
            } else if evt.code != BTN_TOUCH &&
                      !(slots.is_some() && TOUCH_TOOL_KEYS.contains(&evt.code)) {
                if let Some(button_status) = ButtonStatus::try_from_raw(evt.value) {
                    ty.send(DeviceEvent::Button {
                        time: seconds(evt.time),
                        code: ButtonCode::from_raw(evt.code, rotation, button_scheme),
                        status: button_status,
                    }).unwrap();
                }
            }
        } else if evt.kind == EV_MSC && evt.code == MSC_RAW {
            if evt.value >= MSC_RAW_GSENSOR_PORTRAIT_DOWN && evt.value <= MSC_RAW_GSENSOR_LANDSCAPE_LEFT {
                let next_rotation = GYROSCOPE_ROTATIONS.iter().position(|&v| v == evt.value)
                                                       .map(|i| CURRENT_DEVICE.transformed_gyroscope_rotation(i as i8));
                if let Some(next_rotation) = next_rotation {
                    ty.send(DeviceEvent::RotateScreen(next_rotation)).ok();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Kindle PW3's transform: none. `mirror_x`, `mirror_y` false and no
    /// axis swap, so `dims` is never consulted — it is passed anyway so the
    /// mirroring arithmetic stays covered by the mirrored test below.
    const PANEL: (u32, u32) = (1072, 1448);

    fn abs(code: u16, value: i32) -> InputEvent {
        InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            kind: EV_ABS,
            code,
            value,
        }
    }

    fn syn() -> InputEvent {
        InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }
    }

    /// `(status, id, x, y)` — the shape the gesture layer actually consumes.
    type Contact = (FingerStatus, i32, i32, i32);

    fn drive(events: &[InputEvent], mirror_x: bool, mirror_y: bool) -> Vec<Contact> {
        let (ty, ry) = mpsc::channel();
        let mut tracker = MultiSlotTracker::new();
        let tc = MULTI_TOUCH_CODES_SLOT;
        let mut time = 0.0;

        for evt in events {
            match evt.kind {
                EV_ABS => tracker.handle_abs(evt.code, evt.value, &tc, mirror_x, mirror_y, PANEL),
                EV_SYN if evt.code == SYN_REPORT => {
                    time += 0.016;
                    tracker.sync(time, &ty);
                },
                _ => (),
            }
        }

        drop(ty);
        ry.iter().filter_map(|evt| match evt {
            DeviceEvent::Finger { status, id, position, .. } =>
                Some((status, id, position.x, position.y)),
            _ => None,
        }).collect()
    }

    fn run(events: &[InputEvent]) -> Vec<Contact> {
        drive(events, false, false)
    }

    #[test]
    fn single_tap_down_move_up() {
        // Slot 0 is implicit: the PW3 never sends ABS_MT_SLOT for a lone
        // finger (Confirmed in the live capture).
        let events = [
            abs(ABS_MT_TRACKING_ID, 7), abs(ABS_MT_POSITION_X, 100), abs(ABS_MT_POSITION_Y, 200), syn(),
            abs(ABS_MT_POSITION_X, 104), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 7, 100, 200),
            (FingerStatus::Motion, 7, 104, 200),
            (FingerStatus::Up, 7, 104, 200),
        ]);
    }

    #[test]
    fn a_frame_with_no_change_emits_nothing() {
        // The state machine must not mistake "the driver re-sent the same
        // coordinate" for motion, nor emit a Motion per idle SYN_REPORT.
        let events = [
            abs(ABS_MT_TRACKING_ID, 3), abs(ABS_MT_POSITION_X, 10), abs(ABS_MT_POSITION_Y, 20), syn(),
            syn(),
            abs(ABS_MT_POSITION_X, 10), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 3, 10, 20),
            (FingerStatus::Up, 3, 10, 20),
        ]);
    }

    #[test]
    fn two_fingers_with_slot_sent_only_on_change() {
        // The interleaving that breaks a tracking-id-only parser: SLOT is sent
        // when it changes and TRACKING_ID is not repeated, so the second
        // finger's coordinates would otherwise be attributed to the first.
        let events = [
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_TRACKING_ID, 0),
            abs(ABS_MT_POSITION_X, 300), abs(ABS_MT_POSITION_Y, 400), syn(),
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, 1),
            abs(ABS_MT_POSITION_X, 700), abs(ABS_MT_POSITION_Y, 400), syn(),
            // No SLOT here: slot 1 is still current.
            abs(ABS_MT_POSITION_X, 710), syn(),
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_POSITION_X, 290), syn(),
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_TRACKING_ID, -1), syn(),
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 0, 300, 400),
            (FingerStatus::Down, 1, 700, 400),
            (FingerStatus::Motion, 1, 710, 400),
            (FingerStatus::Motion, 0, 290, 400),
            (FingerStatus::Up, 0, 290, 400),
            (FingerStatus::Up, 1, 710, 400),
        ]);
    }

    #[test]
    fn both_fingers_lifting_in_one_frame_keeps_slot_order() {
        let events = [
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_TRACKING_ID, 0),
            abs(ABS_MT_POSITION_X, 1), abs(ABS_MT_POSITION_Y, 2),
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, 1),
            abs(ABS_MT_POSITION_X, 3), abs(ABS_MT_POSITION_Y, 4), syn(),
            // Lifts delivered in reverse slot order inside one packet: the
            // emitted order is still slot order, which is what makes the
            // output deterministic.
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, -1),
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 0, 1, 2),
            (FingerStatus::Down, 1, 3, 4),
            (FingerStatus::Up, 0, 1, 2),
            (FingerStatus::Up, 1, 3, 4),
        ]);
    }

    #[test]
    fn slow_swipe_is_one_contact_with_many_motions() {
        let mut events = vec![
            abs(ABS_MT_TRACKING_ID, 2), abs(ABS_MT_POSITION_X, 100), abs(ABS_MT_POSITION_Y, 700), syn(),
        ];
        for x in 1..=40 {
            events.push(abs(ABS_MT_POSITION_X, 100 + x * 20));
            events.push(syn());
        }
        events.push(abs(ABS_MT_TRACKING_ID, -1));
        events.push(syn());

        let got = run(&events);
        assert_eq!(got.len(), 42);
        assert_eq!(got[0], (FingerStatus::Down, 2, 100, 700));
        assert!(got[1..41].iter().all(|&(status, id, _, y)|
                                      status == FingerStatus::Motion && id == 2 && y == 700));
        assert_eq!(*got.last().unwrap(), (FingerStatus::Up, 2, 900, 700));
    }

    #[test]
    fn a_conservative_driver_never_re_sends_the_tracking_id() {
        // The whole point of protocol B: state persists across SYN_REPORT and
        // only deltas are sent. A parser that needs the id (or a pressure
        // value) every packet sees nothing here after the first frame.
        let events = [
            abs(ABS_MT_TRACKING_ID, 5), abs(ABS_MT_POSITION_X, 50), abs(ABS_MT_POSITION_Y, 60), syn(),
            abs(ABS_MT_POSITION_Y, 61), syn(),
            abs(ABS_MT_POSITION_Y, 62), syn(),
            abs(ABS_MT_POSITION_X, 51), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 5, 50, 60),
            (FingerStatus::Motion, 5, 50, 61),
            (FingerStatus::Motion, 5, 50, 62),
            (FingerStatus::Motion, 5, 51, 62),
            (FingerStatus::Up, 5, 51, 62),
        ]);
    }

    #[test]
    fn a_bare_slot_and_tracking_id_minus_one_lifts_the_finger() {
        // No coordinates in the lift packet: the Up must carry the position
        // the slot was last known to be at.
        let events = [
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, 9),
            abs(ABS_MT_POSITION_X, 640), abs(ABS_MT_POSITION_Y, 480), syn(),
            abs(ABS_MT_SLOT, 1), abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 9, 640, 480),
            (FingerStatus::Up, 9, 640, 480),
        ]);
    }

    #[test]
    fn a_contact_that_begins_and_ends_in_one_frame_is_still_a_tap() {
        let events = [
            abs(ABS_MT_TRACKING_ID, 4), abs(ABS_MT_POSITION_X, 11), abs(ABS_MT_POSITION_Y, 22),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 4, 11, 22),
            (FingerStatus::Up, 4, 11, 22),
        ]);
    }

    #[test]
    fn a_new_tracking_id_without_a_lift_is_an_implicit_lift() {
        let events = [
            abs(ABS_MT_TRACKING_ID, 1), abs(ABS_MT_POSITION_X, 10), abs(ABS_MT_POSITION_Y, 10), syn(),
            abs(ABS_MT_TRACKING_ID, 2), abs(ABS_MT_POSITION_X, 90), abs(ABS_MT_POSITION_Y, 90), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 1, 10, 10),
            (FingerStatus::Up, 1, 10, 10),
            (FingerStatus::Down, 2, 90, 90),
            (FingerStatus::Up, 2, 90, 90),
        ]);
    }

    #[test]
    fn an_out_of_range_slot_is_ignored_not_allocated() {
        let events = [
            abs(ABS_MT_SLOT, 4096), abs(ABS_MT_TRACKING_ID, 1),
            abs(ABS_MT_POSITION_X, 1), abs(ABS_MT_POSITION_Y, 1), syn(),
            abs(ABS_MT_SLOT, 0), abs(ABS_MT_TRACKING_ID, 2),
            abs(ABS_MT_POSITION_X, 5), abs(ABS_MT_POSITION_Y, 6), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(run(&events), vec![
            (FingerStatus::Down, 2, 5, 6),
            (FingerStatus::Up, 2, 5, 6),
        ]);
    }

    #[test]
    fn mirroring_still_applies_to_slot_positions() {
        // Not needed by the PW3 (its transform is the identity) but the code
        // path is shared, so it is pinned rather than left untested.
        let events = [
            abs(ABS_MT_TRACKING_ID, 0), abs(ABS_MT_POSITION_X, 0), abs(ABS_MT_POSITION_Y, 0), syn(),
            abs(ABS_MT_TRACKING_ID, -1), syn(),
        ];
        assert_eq!(drive(&events, true, true), vec![
            (FingerStatus::Down, 0, 1071, 1447),
            (FingerStatus::Up, 0, 1071, 1447),
        ]);
    }

    // --- The live capture -------------------------------------------------

    /// 583 `input_event` structs read off `/dev/input/event1` on the real PW3
    /// (`cyttsp4_mt`), 2026-08-09: taps at TL, TR, BR, BL, a two-finger tap in
    /// the middle and a slow left-to-right swipe — the whole sequence twice.
    /// Provenance: `ezkindle/device-facts/touch-capture-event1.raw`.
    const CAPTURE: &[u8] = include_bytes!("../test-data/touch-capture-event1.raw");

    fn capture_events() -> Vec<InputEvent> {
        assert_eq!(CAPTURE.len() % 16, 0);
        CAPTURE.chunks_exact(16).map(|c| {
            let word = |i: usize| i32::from_le_bytes([c[i], c[i+1], c[i+2], c[i+3]]);
            let half = |i: usize| u16::from_le_bytes([c[i], c[i+1]]);
            InputEvent {
                time: libc::timeval { tv_sec: word(0) as libc::time_t,
                                      tv_usec: word(4) as libc::suseconds_t },
                kind: half(8),
                code: half(10),
                value: word(12),
            }
        }).collect()
    }

    /// A contact, reduced: id, where it went down, where it came up, and how
    /// many motion events it produced.
    #[derive(Debug)]
    struct Stroke {
        id: i32,
        start: (i32, i32),
        end: (i32, i32),
        motions: usize,
        /// Index in the emitted stream, so overlap can be detected.
        down_at: usize,
        up_at: usize,
    }

    fn strokes(contacts: &[Contact]) -> Vec<Stroke> {
        let mut open: Vec<Stroke> = Vec::new();
        let mut done: Vec<Stroke> = Vec::new();
        for (i, &(status, id, x, y)) in contacts.iter().enumerate() {
            match status {
                FingerStatus::Down => open.push(Stroke { id, start: (x, y), end: (x, y),
                                                         motions: 0, down_at: i, up_at: 0 }),
                FingerStatus::Motion => {
                    let s = open.iter_mut().rev().find(|s| s.id == id)
                                .expect("motion for a finger that never went down");
                    s.end = (x, y);
                    s.motions += 1;
                },
                FingerStatus::Up => {
                    let k = open.iter().rposition(|s| s.id == id)
                                .expect("up for a finger that never went down");
                    let mut s = open.remove(k);
                    s.end = (x, y);
                    s.up_at = i;
                    done.push(s);
                },
            }
        }
        assert!(open.is_empty(), "phantom fingers left at end of stream: {:?}", open);
        done
    }

    #[test]
    fn the_live_capture_replays_to_the_gestures_that_were_performed() {
        let contacts = run(&capture_events());
        let strokes = strokes(&contacts);

        // Two rounds of: four corner taps, a two-finger tap, a swipe.
        // 2 * (4 + 2 + 1) = 14 contacts, and every one of them closed.
        assert_eq!(strokes.len(), 14, "{:#?}", strokes);

        // Every coordinate is inside the panel: identity mapping, no scaling.
        for s in &strokes {
            for &(x, y) in &[s.start, s.end] {
                assert!((0..1072).contains(&x) && (0..1448).contains(&y),
                        "off-panel point {:?} in {:?}", (x, y), s);
            }
        }

        // Overlapping contacts = the two-finger taps: exactly two of them,
        // one per round, each a pair.
        let overlapped = |i: usize| {
            let a = &strokes[i];
            strokes.iter().enumerate()
                   .any(|(j, b)| j != i && a.down_at < b.up_at && b.down_at < a.up_at)
        };
        let overlaps = (0..strokes.len()).filter(|&i| overlapped(i)).count();
        assert_eq!(overlaps, 4, "expected two two-finger contacts (4 strokes)");

        // The swipes: a large rightward net displacement, and many motions.
        let mut swipes: Vec<&Stroke> = strokes.iter()
            .filter(|s| s.end.0 - s.start.0 > 200).collect();
        assert_eq!(swipes.len(), 2, "{:#?}", swipes);
        for s in swipes.drain(..) {
            assert!(s.motions > 10, "a swipe should report many motions: {:?}", s);
            assert!(s.end.0 > s.start.0, "swipe should end rightward: {:?}", s);
        }

        // The eight taps: single, isolated, and barely moving. The first four
        // are the corners, in TL, TR, BR, BL order.
        let taps: Vec<&Stroke> = (0..strokes.len())
            .filter(|&i| (strokes[i].end.0 - strokes[i].start.0).abs() <= 200 && !overlapped(i))
            .map(|i| &strokes[i])
            .collect();
        assert_eq!(taps.len(), 8, "{:#?}", taps);
        assert_eq!(taps[0].start, (64, 62), "first tap is the top-left corner");
        assert!(taps[1].start.0 > 900 && taps[1].start.1 < 200, "top-right: {:?}", taps[1]);
        assert!(taps[2].start.0 > 900 && taps[2].start.1 > 1200, "bottom-right: {:?}", taps[2]);
        assert!(taps[3].start.0 < 200 && taps[3].start.1 > 1200, "bottom-left: {:?}", taps[3]);
    }

    #[test]
    fn the_live_capture_needs_slot_state_to_be_read_at_all() {
        // The regression this variant exists to prevent: the stream carries no
        // pressure axis and no ABS_X/ABS_Y, so every pressure-keyed protocol
        // would see a panel nobody ever touched.
        let events = capture_events();
        assert!(!events.iter().any(|e| e.kind == EV_ABS &&
                                   (e.code == ABS_MT_PRESSURE || e.code == ABS_X ||
                                    e.code == ABS_Y || e.code == ABS_PRESSURE)));
        assert!(events.iter().any(|e| e.kind == EV_ABS && e.code == ABS_MT_SLOT));
        // And the tracking id really is sent only on change.
        let ids = events.iter().filter(|e| e.kind == EV_ABS && e.code == ABS_MT_TRACKING_ID).count();
        let syns = events.iter().filter(|e| e.kind == EV_SYN && e.code == SYN_REPORT).count();
        assert!(ids < syns / 4, "{ids} tracking-id events across {syns} packets");
    }
}
