//! `KindleFramebuffer` — the lab126 EPDC backend, for the Paperwhite 3.
//!
//! Structurally this is `kobo1.rs`: mmap `/dev/fb0`, pick pixel accessors from
//! the runtime bpp, address every pixel through `line_length` rather than
//! `xres`. What differs is everything downstream of that — the update struct
//! (`kindle_mxcfb_sys.rs`) and the refresh policy, which is a transcription of
//! **KOReader's PW3 policy**, not of Plato's Kobo one.
//!
//! The policy is factored into [`refresh_policy`], a pure function, so that the
//! part that was written blind is the part that is unit-tested. Everything else
//! in here is `mmap` and `ioctl`.
//!
//! Source for the policy: koreader-base `ffi/framebuffer_mxcfb.lua` @ `ab4cb58`
//! — `refresh_k51`, `mxc_update`, and the `isKindle()`/`isREAGL()` branch of
//! `framebuffer:init()`. Summarised in ezkindle `docs/plato-port.md`.

use std::ptr;
use std::path::Path;
use std::io;
use std::fs::{OpenOptions, File};
use std::slice;
use std::os::unix::io::AsRawFd;
use std::ops::Drop;
use anyhow::{Error, Context, format_err};
use crate::color::Color;
use crate::geom::Rectangle;
use super::{UpdateMode, Framebuffer};
use super::linuxfb_sys::*;
use super::kindle_mxcfb_sys::*;
use super::transform::*;

impl From<Rectangle> for MxcfbRect {
    fn from(rect: Rectangle) -> MxcfbRect {
        MxcfbRect {
            top: rect.min.y as u32,
            left: rect.min.x as u32,
            width: rect.width(),
            height: rect.height(),
        }
    }
}

// ---------------------------------------------------------------------------
// The refresh policy
// ---------------------------------------------------------------------------

/// Everything the policy decides about one update, plus the three fences around
/// it. Pure data, so the decision can be tested without a framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    pub waveform_mode: u32,
    pub update_mode: u32,
    pub hist_bw_waveform_mode: u32,
    pub hist_gray_waveform_mode: u32,
    pub flags: u32,
    /// `MXCFB_WAIT_FOR_UPDATE_SUBMISSION` on the *previous* marker, before the
    /// send. This is the lab126-specific fence; KOReader sets
    /// `wait_for_submission_before = true` for every Kindle.
    pub wait_submission_before: bool,
    /// `MXCFB_WAIT_FOR_UPDATE_COMPLETE` on the *previous* marker, before the send.
    pub wait_complete_before: bool,
    /// `MXCFB_WAIT_FOR_UPDATE_COMPLETE` on *this* update's own marker, after the
    /// send. For a REAGL page turn, the promotion to `UPDATE_MODE_FULL` plus
    /// this wait together *are* the REAGL pacing — dropping either one is what
    /// produces the smeared, racing page turns REAGL exists to avoid.
    pub wait_complete_after: bool,
}

/// KOReader's PW3 refresh policy, as a pure function of Plato's `UpdateMode`
/// and the two sticky framebuffer flags Plato carries.
///
/// The mapping, and where each half comes from:
///
/// | `UpdateMode` | waveform | update mode | fences |
/// |---|---|---|---|
/// | `Gui`      | `GC16_FAST` | `PARTIAL` | submission before |
/// | `Partial`  | `REAGL`     | **`FULL`** (promoted) | complete before, complete after |
/// | `Full`     | `GC16`      | `FULL`    | submission + complete before, complete after |
/// | `Fast`     | `DU`        | `PARTIAL` | — |
/// | `FastMono` | `DU`        | `PARTIAL` | — (+`FORCE_MONOCHROME`) |
///
/// `Fast` is **DU, not A2**: "A2 looks terrible on REAGL devices"
/// (`framebuffer_mxcfb.lua`, the `isREAGL()` branch). The `hist_*` pair is
/// `refresh_k51` verbatim.
pub fn refresh_policy(mode: UpdateMode, monochrome: bool, inverted: bool) -> RefreshPolicy {
    let mut flags = 0;

    // Plato's `inverted` is hardware inversion, exactly as on a Kobo of mark
    // < 11. KOReader instead swaps in GL16_INV (partial) / GC16 (flashing) as
    // its night waveforms; that is a *quality* refinement on top of the same
    // pixels, and it is deliberately left for phase 4 tuning rather than
    // guessed at now. The flag alone is the conservative, reversible choice.
    if inverted {
        flags |= EPDC_FLAG_ENABLE_INVERSION;
    }

    let (mut update_mode, mut waveform_mode) = match mode {
        UpdateMode::Gui => (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_GC16_FAST),
        UpdateMode::Partial => (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_REAGL),
        UpdateMode::Full => (UPDATE_MODE_FULL, WAVEFORM_MODE_GC16),
        UpdateMode::Fast => (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_DU),
        UpdateMode::FastMono => {
            flags |= EPDC_FLAG_FORCE_MONOCHROME;
            (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_DU)
        }
    };

    // `is_flashing`, in KOReader's vocabulary: asked for before any promotion,
    // because the fence heuristics below key on the *requested* intent.
    let is_flashing = mode == UpdateMode::Full;

    // Sticky monochrome (Plato's `set_monochrome`) degrades anything that isn't
    // already a full flash to DU + FORCE_MONOCHROME. Same shape as kobo1.rs's
    // mark >= 7 branch, with DU in place of A2 for the reason above.
    if monochrome && !is_flashing {
        waveform_mode = WAVEFORM_MODE_DU;
        update_mode = UPDATE_MODE_PARTIAL;
        flags |= EPDC_FLAG_FORCE_MONOCHROME;
    }

    // refresh_k51, verbatim.
    let (hist_bw_waveform_mode, hist_gray_waveform_mode) =
        if waveform_mode == WAVEFORM_MODE_REAGL {
            // "If we're requesting WAVEFORM_MODE_REAGL, it's REAGL all around!"
            (WAVEFORM_MODE_REAGL, WAVEFORM_MODE_REAGL)
        } else if waveform_mode == WAVEFORM_MODE_GC16 {
            (WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16)
        } else {
            (WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16_FAST)
        };

    let is_reagl = waveform_mode == WAVEFORM_MODE_REAGL;

    // mxc_update: "REAGL updates (almost) always need to be full."
    if is_reagl {
        update_mode = UPDATE_MODE_FULL;
    }

    // mxc_update, the two pre-send fences. `wait_for_submission_before` is set
    // for every Kindle; `_isUIWaveFormMode` is GC16_FAST here, and
    // `waveform_flashui == waveform_ui`, so a flashing UI update is one too.
    let wait_submission_before = is_flashing || waveform_mode == WAVEFORM_MODE_GC16_FAST;
    // "If we're trying to send a REAGL update, a GC16 update, or a full-screen
    // flashing UI update, then wait for completion of previous marker first."
    // The full-screen test is not reproduced: Plato's `Full` is already the
    // flashing intent, and it is GC16, so the first two clauses cover it.
    let wait_complete_before = is_reagl || waveform_mode == WAVEFORM_MODE_GC16;

    RefreshPolicy {
        waveform_mode,
        update_mode,
        hist_bw_waveform_mode,
        hist_gray_waveform_mode,
        flags,
        wait_submission_before,
        wait_complete_before,
        // "We want to fence off FULL updates" — keyed on the *post*-promotion
        // update mode, so REAGL page turns are fenced too.
        wait_complete_after: update_mode == UPDATE_MODE_FULL,
    }
}

// ---------------------------------------------------------------------------
// The framebuffer
// ---------------------------------------------------------------------------

type SetPixelRgb = fn(&mut KindleFramebuffer, u32, u32, [u8; 3]);
type GetPixelRgb = fn(&KindleFramebuffer, u32, u32) -> [u8; 3];
type AsRgb = fn(&KindleFramebuffer) -> Vec<u8>;

pub struct KindleFramebuffer {
    file: File,
    frame: *mut libc::c_void,
    frame_size: libc::size_t,
    token: u32,
    /// A marker we have already blocked on, so `wait` does not block on it
    /// twice. KOReader's `dont_wait_for_marker`.
    settled_token: u32,
    monochrome: bool,
    dithered: bool,
    inverted: bool,
    /// The panel's one true rotation. See `set_rotation`.
    rotation: i8,
    transform: ColorTransform,
    set_pixel_rgb: SetPixelRgb,
    get_pixel_rgb: GetPixelRgb,
    as_rgb: AsRgb,
    red_index: usize,
    green_index: usize,
    blue_index: usize,
    bytes_per_pixel: u8,
    var_info: VarScreenInfo,
    fix_info: FixScreenInfo,
}

impl KindleFramebuffer {
    pub fn new<P: AsRef<Path>>(path: P, rotation: i8) -> Result<KindleFramebuffer, Error> {
        let file = OpenOptions::new().read(true)
                                     .write(true)
                                     .open(&path)
                                     .with_context(|| format!("can't open framebuffer device {}", path.as_ref().display()))?;

        let var_info = var_screen_info(&file)?;
        let fix_info = fix_screen_info(&file)?;

        // The PW3 is expected to be 8bpp with a 1088-byte stride for 1072
        // columns — but that is Likely, not Confirmed, so the 16/32bpp paths
        // are kept exactly as kobo1.rs has them and the stride is always read
        // from `line_length`. See PLATO-DEVICE-PROBES.
        if var_info.bits_per_pixel % 8 != 0 {
            return Err(format_err!("unsupported framebuffer depth: {} bits per pixel",
                                   var_info.bits_per_pixel));
        }

        let bytes_per_pixel = var_info.bits_per_pixel / 8;
        let frame_size = (var_info.yres * fix_info.line_length) as libc::size_t;

        let frame = unsafe {
            libc::mmap(ptr::null_mut(), fix_info.smem_len as usize,
                       libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
                       file.as_raw_fd(), 0)
        };

        if frame == libc::MAP_FAILED {
            return Err(Error::from(io::Error::last_os_error()).context("can't map memory"));
        }

        let (set_pixel_rgb, get_pixel_rgb, as_rgb): (SetPixelRgb, GetPixelRgb, AsRgb) =
            if var_info.bits_per_pixel > 16 {
                (set_pixel_rgb_32, get_pixel_rgb_32, as_rgb_32)
            } else if var_info.bits_per_pixel > 8 {
                (set_pixel_rgb_16, get_pixel_rgb_16, as_rgb_16)
            } else {
                (set_pixel_rgb_8, get_pixel_rgb_8, as_rgb_8)
            };
        let red_index = if var_info.red.offset > 0 { 2 } else { 0 };

        Ok(KindleFramebuffer {
            file,
            frame,
            frame_size,
            token: 1,
            settled_token: 0,
            monochrome: false,
            dithered: false,
            inverted: false,
            rotation,
            transform: transform_identity,
            set_pixel_rgb,
            get_pixel_rgb,
            as_rgb,
            red_index,
            green_index: 1,
            blue_index: 2 - red_index,
            bytes_per_pixel: bytes_per_pixel as u8,
            var_info,
            fix_info,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.frame as *const u8, self.frame_size) }
    }

    fn next_token(&mut self) -> u32 {
        let token = self.token;
        // 0 is our "nothing to wait for" sentinel, so skip it on wrap.
        self.token = match self.token.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        token
    }

    fn wait_complete(&self, token: u32) -> Result<(), Error> {
        if token == 0 {
            return Ok(());
        }
        let mut marker_data = MxcfbUpdateMarkerData {
            update_marker: token,
            // "0 seems to be a fairly safe assumption for we don't care about
            // collisions." Never EPDC_FLAG_TEST_COLLISION.
            collision_test: 0,
        };
        let ret = unsafe {
            libc::ioctl(self.file.as_raw_fd(),
                        MXCFB_WAIT_FOR_UPDATE_COMPLETE as _,
                        &mut marker_data as *mut MxcfbUpdateMarkerData)
        };
        if ret == -1 {
            Err(Error::from(io::Error::last_os_error())
                    .context("can't wait for framebuffer update to complete"))
        } else {
            Ok(())
        }
    }

    fn wait_submission(&self, token: u32) -> Result<(), Error> {
        if token == 0 {
            return Ok(());
        }
        let ret = unsafe {
            libc::ioctl(self.file.as_raw_fd(),
                        MXCFB_WAIT_FOR_UPDATE_SUBMISSION as _,
                        &token as *const u32)
        };
        if ret == -1 {
            Err(Error::from(io::Error::last_os_error())
                    .context("can't wait for framebuffer update submission"))
        } else {
            Ok(())
        }
    }
}

impl Framebuffer for KindleFramebuffer {
    fn set_pixel(&mut self, x: u32, y: u32, color: Color) {
        let c = (self.transform)(x, y, color);
        (self.set_pixel_rgb)(self, x, y, c.rgb());
    }

    fn set_blended_pixel(&mut self, x: u32, y: u32, color: Color, alpha: f32) {
        if alpha >= 1.0 {
            self.set_pixel(x, y, color);
            return;
        }
        let background = Color::from_rgb(&(self.get_pixel_rgb)(self, x, y));
        let interp = background.lerp(color, alpha);
        let c = (self.transform)(x, y, interp);
        (self.set_pixel_rgb)(self, x, y, c.rgb());
    }

    fn invert_region(&mut self, rect: &Rectangle) {
        for y in rect.min.y..rect.max.y {
            for x in rect.min.x..rect.max.x {
                let rgb = (self.get_pixel_rgb)(self, x as u32, y as u32);
                let color = [255 - rgb[0], 255 - rgb[1], 255 - rgb[2]];
                (self.set_pixel_rgb)(self, x as u32, y as u32, color);
            }
        }
    }

    fn shift_region(&mut self, rect: &Rectangle, drift: u8) {
        for y in rect.min.y..rect.max.y {
            for x in rect.min.x..rect.max.x {
                let rgb = (self.get_pixel_rgb)(self, x as u32, y as u32);
                let color = [rgb[0].saturating_sub(drift), rgb[1].saturating_sub(drift), rgb[2].saturating_sub(drift)];
                (self.set_pixel_rgb)(self, x as u32, y as u32, color);
            }
        }
    }

    fn update(&mut self, rect: &Rectangle, mode: UpdateMode) -> Result<u32, Error> {
        // "Discard bogus refresh region" — koreader/koreader#1299 and #1486.
        // A 1px-wide or 1px-tall update murders this kernel's EPDC.
        if rect.width() <= 1 || rect.height() <= 1 {
            return Ok(0);
        }

        let policy = refresh_policy(mode, self.monochrome, self.inverted);
        let previous = self.token.wrapping_sub(1);
        let previous = if previous == self.settled_token { 0 } else { previous };

        if policy.wait_submission_before {
            self.wait_submission(previous)
                .map_err(|e| eprintln!("{:#}.", e)).ok();
        }
        if policy.wait_complete_before {
            self.wait_complete(previous)
                .map_err(|e| eprintln!("{:#}.", e)).ok();
        }

        let update_marker = self.next_token();
        let update_data = MxcfbUpdateData {
            update_region: (*rect).into(),
            waveform_mode: policy.waveform_mode,
            update_mode: policy.update_mode,
            update_marker,
            hist_bw_waveform_mode: policy.hist_bw_waveform_mode,
            hist_gray_waveform_mode: policy.hist_gray_waveform_mode,
            // Set on every send rather than once at init: the struct is built
            // fresh here, so there is no "once" to speak of.
            temp: TEMP_USE_AUTO,
            flags: policy.flags,
            alt_buffer_data: MxcfbAltBufferData::default(),
        };

        let ret = unsafe {
            libc::ioctl(self.file.as_raw_fd(),
                        MXCFB_SEND_UPDATE as _,
                        &update_data as *const MxcfbUpdateData)
        };

        if ret == -1 {
            return Err(Error::from(io::Error::last_os_error())
                           .context("can't send framebuffer update"));
        }

        if policy.wait_complete_after {
            self.wait_complete(update_marker)
                .map_err(|e| eprintln!("{:#}.", e)).ok();
            self.settled_token = update_marker;
        }

        Ok(update_marker)
    }

    fn wait(&self, token: u32) -> Result<i32, Error> {
        if token == 0 || token == self.settled_token {
            return Ok(0);
        }
        self.wait_complete(token).map(|_| 0)
    }

    fn save(&self, path: &str) -> Result<(), Error> {
        let (width, height) = self.dims();
        let file = File::create(path).with_context(|| format!("can't create output file {}", path))?;
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().with_context(|| format!("can't write PNG header for {}", path))?;
        writer.write_image_data(&(self.as_rgb)(self)).with_context(|| format!("can't write PNG data to {}", path))?;
        Ok(())
    }

    #[inline]
    fn rotation(&self) -> i8 {
        self.rotation
    }

    /// **The PW3 does not rotate.** Not "we haven't implemented it": KOReader
    /// treats every Kindle framebuffer as fixed-orientation and rotates in
    /// software, and writing `var_info.rotate` through `FBIOPUT_VSCREENINFO` on
    /// a lab126 EPDC is not a thing Amazon's own reader ever does.
    ///
    /// So this backend never writes rotation, and the `Device` ladder is set up
    /// (`startup_rotation() == 0`, `swapping_scheme() == 0`, default mirroring)
    /// so that the native rotation *is* portrait with an untransformed touch
    /// panel — the two facts KOReader records for the PW3.
    ///
    /// A request for a different rotation returns `Err`. That is the minimal
    /// correct behaviour, and it is correct rather than merely convenient
    /// because of how every consumer is written: `app.rs` guards all six of its
    /// calls with `if let Ok(dims) = …`, so an `Err` leaves `context.display`
    /// untouched — dims, rotation and therefore the input-side axis transform
    /// all stay consistent with the panel. Returning `Ok` with unchanged dims
    /// would instead let the caller record a rotation the hardware does not
    /// have, and the touch mapping would silently follow it.
    ///
    /// Real software rotation (transposing in `set_pixel`) is a later,
    /// self-contained addition; nothing about this choice blocks it.
    fn set_rotation(&mut self, n: i8) -> Result<(u32, u32), Error> {
        if n == self.rotation {
            return Ok((self.var_info.xres, self.var_info.yres));
        }
        Err(format_err!("this device's panel is fixed at rotation {}; \
                         software rotation isn't implemented", self.rotation))
    }

    fn set_inverted(&mut self, enable: bool) {
        self.inverted = enable;
    }

    fn inverted(&self) -> bool {
        self.inverted
    }

    fn set_monochrome(&mut self, enable: bool) {
        self.monochrome = enable;
    }

    fn monochrome(&self) -> bool {
        self.monochrome
    }

    /// Software dithering only, like a Kobo of mark < 7.
    ///
    /// The lab126 dialect *has* `EPDC_FLAG_USE_DITHERING_Y1`/`_Y4`, but
    /// KOReader never enables hardware dithering on any Kindle, and the
    /// alternative here is a blind guess about a PxP path we cannot observe.
    /// `transform_dither_g16` costs CPU and nothing else.
    fn set_dithered(&mut self, enable: bool) {
        if enable == self.dithered {
            return;
        }
        self.dithered = enable;
        self.transform = if enable { transform_dither_g16 } else { transform_identity };
    }

    fn dithered(&self) -> bool {
        self.dithered
    }

    fn width(&self) -> u32 {
        self.var_info.xres
    }

    fn height(&self) -> u32 {
        self.var_info.yres
    }
}

impl Drop for KindleFramebuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.frame, self.fix_info.smem_len as usize);
        }
    }
}

// The pixel accessors are kobo1.rs's, verbatim apart from the receiver type.
// They address through `line_length`, which is the only part that matters here:
// the PW3's stride is expected to be 1088 for 1072 columns.

fn set_pixel_rgb_8(fb: &mut KindleFramebuffer, x: u32, y: u32, rgb: [u8; 3]) {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);

    debug_assert!(addr < fb.frame_size as isize);

    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        *spot = rgb[0];
    }
}

fn set_pixel_rgb_16(fb: &mut KindleFramebuffer, x: u32, y: u32, rgb: [u8; 3]) {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);

    debug_assert!(addr < fb.frame_size as isize);

    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        *spot.offset(0) = rgb[2] >> 3 | (rgb[1] & 0b0001_1100) << 3;
        *spot.offset(1) = (rgb[0] & 0b1111_1000) | rgb[1] >> 5;
    }
}

fn set_pixel_rgb_32(fb: &mut KindleFramebuffer, x: u32, y: u32, rgb: [u8; 3]) {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);

    debug_assert!(addr < fb.frame_size as isize);

    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        *spot.offset(0) = rgb[fb.red_index];
        *spot.offset(1) = rgb[fb.green_index];
        *spot.offset(2) = rgb[fb.blue_index];
    }
}

fn get_pixel_rgb_8(fb: &KindleFramebuffer, x: u32, y: u32) -> [u8; 3] {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);
    let gray = unsafe { *(fb.frame.offset(addr) as *const u8) };
    [gray, gray, gray]
}

fn get_pixel_rgb_16(fb: &KindleFramebuffer, x: u32, y: u32) -> [u8; 3] {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);
    let pair = unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        [*spot.offset(0), *spot.offset(1)]
    };
    let red = pair[1] & 0b1111_1000;
    let green = ((pair[1] & 0b0000_0111) << 5) | ((pair[0] & 0b1110_0000) >> 3);
    let blue = (pair[0] & 0b0001_1111) << 3;
    [red, green, blue]
}

fn get_pixel_rgb_32(fb: &KindleFramebuffer, x: u32, y: u32) -> [u8; 3] {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);
    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        [*spot.offset(fb.red_index as isize),
         *spot.offset(fb.green_index as isize),
         *spot.offset(fb.blue_index as isize)]
    }
}

fn as_rgb_8(fb: &KindleFramebuffer) -> Vec<u8> {
    let (width, height) = fb.dims();
    let mut rgb888 = Vec::with_capacity((width * height * 3) as usize);
    let rgb8 = fb.as_bytes();
    let virtual_width = fb.var_info.xres_virtual as usize;
    for (_, &gray) in rgb8.iter().take(height as usize * virtual_width).enumerate()
                          .filter(|&(i, _)| i % virtual_width < width as usize) {
        rgb888.extend_from_slice(&[gray, gray, gray]);
    }
    rgb888
}

fn as_rgb_16(fb: &KindleFramebuffer) -> Vec<u8> {
    let (width, height) = fb.dims();
    let mut rgb888 = Vec::with_capacity((width * height * 3) as usize);
    let rgb565 = fb.as_bytes();
    let virtual_width = fb.var_info.xres_virtual as usize;
    for (_, pair) in rgb565.chunks(2).take(height as usize * virtual_width).enumerate()
                           .filter(|&(i, _)| i % virtual_width < width as usize) {
        let red = pair[1] & 0b1111_1000;
        let green = ((pair[1] & 0b0000_0111) << 5) | ((pair[0] & 0b1110_0000) >> 3);
        let blue = (pair[0] & 0b0001_1111) << 3;
        rgb888.extend_from_slice(&[red, green, blue]);
    }
    rgb888
}

fn as_rgb_32(fb: &KindleFramebuffer) -> Vec<u8> {
    let (width, height) = fb.dims();
    let mut rgb888 = Vec::with_capacity((width * height * 3) as usize);
    let data = fb.as_bytes();
    let virtual_width = fb.var_info.xres_virtual as usize;
    for (_, color) in data.chunks(4).take(height as usize * virtual_width).enumerate()
                          .filter(|&(i, _)| i % virtual_width < width as usize) {
        let red = color[fb.red_index];
        let green = color[fb.green_index];
        let blue = color[fb.blue_index];
        rgb888.extend_from_slice(&[red, green, blue]);
    }
    rgb888
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(mode: UpdateMode) -> RefreshPolicy {
        refresh_policy(mode, false, false)
    }

    #[test]
    fn page_turns_are_reagl_promoted_to_full_and_fenced() {
        let p = plain(UpdateMode::Partial);
        assert_eq!(p.waveform_mode, WAVEFORM_MODE_REAGL);
        // The promotion and the wait together are the REAGL pacing.
        assert_eq!(p.update_mode, UPDATE_MODE_FULL);
        assert!(p.wait_complete_before);
        assert!(p.wait_complete_after);
        assert!(!p.wait_submission_before);
        // "It's REAGL all around."
        assert_eq!(p.hist_bw_waveform_mode, WAVEFORM_MODE_REAGL);
        assert_eq!(p.hist_gray_waveform_mode, WAVEFORM_MODE_REAGL);
        assert_eq!(p.flags, 0);
    }

    #[test]
    fn gui_is_a_gc16_fast_partial_that_waits_for_submission() {
        let p = plain(UpdateMode::Gui);
        assert_eq!(p.waveform_mode, WAVEFORM_MODE_GC16_FAST);
        assert_eq!(p.update_mode, UPDATE_MODE_PARTIAL);
        assert!(p.wait_submission_before);
        assert!(!p.wait_complete_before);
        assert!(!p.wait_complete_after);
        assert_eq!(p.hist_bw_waveform_mode, WAVEFORM_MODE_DU);
        assert_eq!(p.hist_gray_waveform_mode, WAVEFORM_MODE_GC16_FAST);
    }

    #[test]
    fn full_is_gc16_full_and_fenced_both_ways() {
        let p = plain(UpdateMode::Full);
        assert_eq!(p.waveform_mode, WAVEFORM_MODE_GC16);
        assert_eq!(p.update_mode, UPDATE_MODE_FULL);
        assert!(p.wait_submission_before);
        assert!(p.wait_complete_before);
        assert!(p.wait_complete_after);
        // GC16 special case: hist_gray follows the waveform, hist_bw stays DU.
        assert_eq!(p.hist_bw_waveform_mode, WAVEFORM_MODE_DU);
        assert_eq!(p.hist_gray_waveform_mode, WAVEFORM_MODE_GC16);
    }

    #[test]
    fn fast_is_du_not_a2() {
        for mode in [UpdateMode::Fast, UpdateMode::FastMono] {
            let p = plain(mode);
            assert_eq!(p.waveform_mode, WAVEFORM_MODE_DU,
                       "A2 looks terrible on REAGL devices");
            assert_ne!(p.waveform_mode, WAVEFORM_MODE_A2);
            assert_eq!(p.update_mode, UPDATE_MODE_PARTIAL);
            assert!(!p.wait_submission_before);
            assert!(!p.wait_complete_before);
            assert!(!p.wait_complete_after);
        }
        assert_eq!(plain(UpdateMode::Fast).flags, 0);
        assert_eq!(plain(UpdateMode::FastMono).flags, EPDC_FLAG_FORCE_MONOCHROME);
    }

    #[test]
    fn no_update_ever_sets_the_collision_test_flag() {
        for mode in [UpdateMode::Gui, UpdateMode::Partial, UpdateMode::Full,
                     UpdateMode::Fast, UpdateMode::FastMono] {
            for &mono in &[false, true] {
                for &inv in &[false, true] {
                    let p = refresh_policy(mode, mono, inv);
                    assert_eq!(p.flags & EPDC_FLAG_TEST_COLLISION, 0);
                    // No hardware dithering, ever.
                    assert_eq!(p.flags & (EPDC_FLAG_USE_DITHERING_Y1 |
                                          EPDC_FLAG_USE_DITHERING_Y2 |
                                          EPDC_FLAG_USE_DITHERING_Y4), 0);
                }
            }
        }
    }

    #[test]
    fn monochrome_degrades_everything_but_a_full_flash_to_du() {
        for mode in [UpdateMode::Gui, UpdateMode::Partial, UpdateMode::Fast] {
            let p = refresh_policy(mode, true, false);
            assert_eq!(p.waveform_mode, WAVEFORM_MODE_DU);
            assert_eq!(p.update_mode, UPDATE_MODE_PARTIAL);
            assert_eq!(p.flags & EPDC_FLAG_FORCE_MONOCHROME, EPDC_FLAG_FORCE_MONOCHROME);
        }
        let p = refresh_policy(UpdateMode::Full, true, false);
        assert_eq!(p.waveform_mode, WAVEFORM_MODE_GC16);
        assert_eq!(p.update_mode, UPDATE_MODE_FULL);
    }

    #[test]
    fn inversion_is_a_flag_and_nothing_else() {
        for mode in [UpdateMode::Gui, UpdateMode::Partial, UpdateMode::Full,
                     UpdateMode::Fast, UpdateMode::FastMono] {
            let off = refresh_policy(mode, false, false);
            let on = refresh_policy(mode, false, true);
            assert_eq!(on.flags, off.flags | EPDC_FLAG_ENABLE_INVERSION);
            assert_eq!(on.waveform_mode, off.waveform_mode);
            assert_eq!(on.update_mode, off.update_mode);
        }
    }

    #[test]
    fn temperature_is_the_lab126_auto_value() {
        assert_eq!(TEMP_USE_AUTO, 0x1001);
        assert_ne!(TEMP_USE_AUTO, TEMP_USE_AMBIENT);
    }
}
