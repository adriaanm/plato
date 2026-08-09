//! The **lab126** mxcfb dialect, as shipped on Amazon's `3.0.35-lab126`
//! kernels — here specifically the Paperwhite 3 (`muscat`, i.MX6SL Wario,
//! FW 5.16.2.1.1).
//!
//! This is a sibling of `mxcfb_sys.rs`, which is the Kobo/NTX dialect. The two
//! share an ancestor and a great many constants, but **not the update struct**,
//! and therefore not the size nibble of `MXCFB_SEND_UPDATE`. Getting that wrong
//! does not fail at the `ioctl` boundary with `EINVAL`; it fails as a hung EPDC
//! or a garbage frame, on a device we cannot single-step. So every number here
//! is asserted at compile time against a value read out of a header or out of
//! KOReader, and the build fails if any of them drifts.
//!
//! Sources, both read in full and cross-checked against each other:
//!
//! * koreader-base `ffi-cdecl/include/mxcfb-kindle.h` @ `ab4cb58` — Amazon's
//!   own `uapi/linux/mxcfb.h`, reconstructed by NiLuJe across firmware
//!   generations. **This is the authority**; where the port notes and the
//!   header disagree, the header wins.
//! * ezkindle `docs/plato-port.md` — the delta table.
//!
//! Nothing in this file is Kobo-compatible and nothing in `mxcfb_sys.rs` is
//! Kindle-compatible. They are deliberately kept apart rather than unified
//! behind `cfg`s or a `mark()` ladder: the whole risk here is a struct that
//! silently means something else.

#![allow(unused)]

use std::mem::size_of;

/// The `_IOC` encoding, **Linux's**, written out rather than taken from `nix`.
///
/// `nix`'s `ioctl_*!` macros are correct on Linux but encode BSD-style on
/// macOS, so a compile-time assertion on their output would assert something
/// different on the host than on the device — i.e. it would check nothing where
/// it matters. These constants are the numbers the device sees, on every host.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (ty << 8) | nr
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_READ_WRITE: u32 = 3;

/// `'F'`, the mxcfb ioctl magic — same on both dialects.
const MAGIC: u32 = 0x46;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbRect {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

/// lab126's alt buffer: **no `virt_addr`**, unlike the Kobo V1 struct.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbAltBufferData {
    pub phys_addr: u32,
    /// Width of the entire buffer.
    pub width: u32,
    /// Height of the entire buffer.
    pub height: u32,
    /// Region within the buffer to update.
    pub alt_update_region: MxcfbRect,
}

/// `struct mxcfb_update_data`, lab126 flavour.
///
/// The delta against the Kobo V1 struct is exactly two `uint32_t` — the
/// `hist_*_waveform_mode` pair, which lab126 wedged **between `update_marker`
/// and `temp`** rather than appending. That both moves every following field
/// and takes the struct from 68 to 72 bytes, which is why the send ioctl's
/// number differs while its magic and nr do not.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbUpdateData {
    pub update_region: MxcfbRect,
    pub waveform_mode: u32,
    pub update_mode: u32,
    pub update_marker: u32,
    /// Lab126: default b&w waveform for histogram analysis.
    pub hist_bw_waveform_mode: u32,
    /// Lab126: default gray waveform for histogram analysis.
    pub hist_gray_waveform_mode: u32,
    pub temp: i32,
    pub flags: u32,
    pub alt_buffer_data: MxcfbAltBufferData,
}

/// `struct mxcfb_update_marker_data` — byte-identical to the Kobo V2 one, which
/// is why the PW3's wait ioctl is *the same number* as Plato's existing
/// `wait_for_update_v2`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbUpdateMarkerData {
    pub update_marker: u32,
    /// `0` means "we don't care about collisions". Never set
    /// `EPDC_FLAG_TEST_COLLISION`; that is a dry-run collision test.
    pub collision_test: u32,
}

// ---------------------------------------------------------------------------
// ioctls
// ---------------------------------------------------------------------------

pub const MXCFB_SET_TEMPERATURE: u32 = ioc(IOC_WRITE, MAGIC, 0x2C, 4);
pub const MXCFB_SET_AUTO_UPDATE_MODE: u32 = ioc(IOC_WRITE, MAGIC, 0x2D, 4);
pub const MXCFB_SEND_UPDATE: u32 =
    ioc(IOC_WRITE, MAGIC, 0x2E, size_of::<MxcfbUpdateData>() as u32);
pub const MXCFB_WAIT_FOR_UPDATE_COMPLETE: u32 =
    ioc(IOC_READ_WRITE, MAGIC, 0x2F, size_of::<MxcfbUpdateMarkerData>() as u32);
pub const MXCFB_SET_PWRDOWN_DELAY: u32 = ioc(IOC_WRITE, MAGIC, 0x30, 4);
pub const MXCFB_GET_PWRDOWN_DELAY: u32 = ioc(IOC_READ, MAGIC, 0x31, 4);
pub const MXCFB_SET_UPDATE_SCHEME: u32 = ioc(IOC_WRITE, MAGIC, 0x32, 4);
pub const MXCFB_WAIT_FOR_UPDATE_SUBMISSION: u32 = ioc(IOC_WRITE, MAGIC, 0x37, 4);
pub const MXCFB_GET_TEMPERATURE: u32 = ioc(IOC_READ, MAGIC, 0x38, 4);
pub const MXCFB_GET_WAVEFORM_TYPE: u32 = ioc(IOC_READ, MAGIC, 0x39, 4);

// ---------------------------------------------------------------------------
// The blind safety net
// ---------------------------------------------------------------------------
//
// Known-good values. The first three are quoted verbatim in KOReader's
// `ffi/framebuffer_mxcfb.lua` ("Kindle's MXCFB_WAIT_FOR_UPDATE_SUBMISSION ==
// 0x40044637") and in ezkindle's docs/plato-port.md delta table, i.e. they were
// obtained independently of this file's arithmetic.

const _: () = assert!(size_of::<MxcfbRect>() == 16);
const _: () = assert!(size_of::<MxcfbAltBufferData>() == 28);
const _: () = assert!(size_of::<MxcfbUpdateData>() == 72);
const _: () = assert!(size_of::<MxcfbUpdateMarkerData>() == 8);

// 0x48 == 72: the size nibble *is* the struct size, so this one assertion ties
// the ioctl number and the layout together. The Kobo number is 0x4044462e.
const _: () = assert!(MXCFB_SEND_UPDATE == 0x4048_462e);
const _: () = assert!(MXCFB_WAIT_FOR_UPDATE_COMPLETE == 0xc008_462f);
const _: () = assert!(MXCFB_WAIT_FOR_UPDATE_SUBMISSION == 0x4004_4637);
const _: () = assert!(MXCFB_SET_TEMPERATURE == 0x4004_462c);
const _: () = assert!(MXCFB_SET_AUTO_UPDATE_MODE == 0x4004_462d);
const _: () = assert!(MXCFB_SET_UPDATE_SCHEME == 0x4004_4632);
const _: () = assert!(MXCFB_GET_TEMPERATURE == 0x8004_4638);
const _: () = assert!(MXCFB_GET_WAVEFORM_TYPE == 0x8004_4639);

// Field offsets, so that "the struct is 72 bytes" cannot be satisfied by the
// *wrong* 72 bytes (e.g. the hist pair appended after `flags`, as the Rex
// variant does).
const _: () = {
    assert!(std::mem::offset_of!(MxcfbUpdateData, waveform_mode) == 16);
    assert!(std::mem::offset_of!(MxcfbUpdateData, update_mode) == 20);
    assert!(std::mem::offset_of!(MxcfbUpdateData, update_marker) == 24);
    assert!(std::mem::offset_of!(MxcfbUpdateData, hist_bw_waveform_mode) == 28);
    assert!(std::mem::offset_of!(MxcfbUpdateData, hist_gray_waveform_mode) == 32);
    assert!(std::mem::offset_of!(MxcfbUpdateData, temp) == 36);
    assert!(std::mem::offset_of!(MxcfbUpdateData, flags) == 40);
    assert!(std::mem::offset_of!(MxcfbUpdateData, alt_buffer_data) == 44);
};

// ---------------------------------------------------------------------------
// Waveform modes
// ---------------------------------------------------------------------------
//
// INIT/DU/GC16/A2/GL16/AUTO share their values with the NTX dialect; the REAGL
// family does *not* (Kobo: GLR16=6, GLD16=7).

/// Screen goes to white (clears).
pub const WAVEFORM_MODE_INIT: u32 = 0x0;
/// Grey→white / grey→black.
pub const WAVEFORM_MODE_DU: u32 = 0x1;
/// High fidelity (flashing).
pub const WAVEFORM_MODE_GC16: u32 = 0x2;
/// Medium fidelity.
pub const WAVEFORM_MODE_GC16_FAST: u32 = 0x3;
/// Faster but even lower fidelity. Unused here — A2 looks terrible on REAGL
/// devices, so KOReader uses DU for its "fast" waveform on the PW3, and so do we.
pub const WAVEFORM_MODE_A2: u32 = 0x4;
/// High fidelity from white transition.
pub const WAVEFORM_MODE_GL16: u32 = 0x5;
/// Medium fidelity from white transition.
pub const WAVEFORM_MODE_GL16_FAST: u32 = 0x6;
/// FW >= 5.3. Medium fidelity, 4 levels of gray, direct update.
pub const WAVEFORM_MODE_DU4: u32 = 0x7;
/// PW2/KT2/KV. Ghost compensation waveform — the page-turn waveform here.
pub const WAVEFORM_MODE_REAGL: u32 = 0x8;
/// Ghost compensation waveform with dithering.
pub const WAVEFORM_MODE_REAGLD: u32 = 0x9;
/// KT2/KV. 2-bit from white transition.
pub const WAVEFORM_MODE_GL4: u32 = 0xA;
/// KT2/KV. High fidelity for black transition — KOReader's night-mode partial
/// waveform on REAGL Kindles. Not used yet; see `kindle.rs` on inversion.
pub const WAVEFORM_MODE_GL16_INV: u32 = 0xB;
/// Let the driver pick. Same value as the NTX dialect's.
pub const WAVEFORM_MODE_AUTO: u32 = 257;

const _: () = assert!(WAVEFORM_MODE_AUTO == 0x101);

// ---------------------------------------------------------------------------
// Update modes, temperature, flags
// ---------------------------------------------------------------------------

pub const UPDATE_MODE_PARTIAL: u32 = 0x0;
pub const UPDATE_MODE_FULL: u32 = 0x1;

pub const AUTO_UPDATE_MODE_REGION_MODE: u32 = 0;
pub const AUTO_UPDATE_MODE_AUTOMATIC_MODE: u32 = 1;

pub const UPDATE_SCHEME_SNAPSHOT: u32 = 0;
pub const UPDATE_SCHEME_QUEUE: u32 = 1;
pub const UPDATE_SCHEME_QUEUE_AND_MERGE: u32 = 2;

pub const TEMP_USE_AMBIENT: i32 = 0x1000;
/// PW2 and up. Note this is `TEMP_USE_PAPYRUS` on Touch/PW1 — same value, so
/// KOReader sets it unconditionally on the k51 path, and so do we.
pub const TEMP_USE_AUTO: i32 = 0x1001;

pub const EPDC_FLAG_ENABLE_INVERSION: u32 = 0x01;
pub const EPDC_FLAG_FORCE_MONOCHROME: u32 = 0x02;
pub const EPDC_FLAG_USE_CMAP: u32 = 0x04;
pub const EPDC_FLAG_USE_ALT_BUFFER: u32 = 0x100;
/// Dry-run collision test. **Never set this.**
pub const EPDC_FLAG_TEST_COLLISION: u32 = 0x200;
pub const EPDC_FLAG_GROUP_UPDATE: u32 = 0x400;
pub const EPDC_FLAG_FORCE_Y2: u32 = 0x800;
pub const EPDC_FLAG_USE_REAGLD: u32 = 0x1000;
/// Hardware dithering. Unused: KOReader never enables HW dithering on lab126,
/// and `KindleFramebuffer` dithers in software instead.
pub const EPDC_FLAG_USE_DITHERING_Y1: u32 = 0x2000;
pub const EPDC_FLAG_USE_DITHERING_Y2: u32 = 0x4000;
pub const EPDC_FLAG_USE_DITHERING_Y4: u32 = 0x8000;

/// Returned by `MXCFB_GET_WAVEFORM_TYPE`.
pub const WAVEFORM_TYPE_4BIT: u32 = 0x1;
pub const WAVEFORM_TYPE_5BIT: u32 = 0x2;

/// `GRAYSCALE_8BIT` — the `grayscale` field of `fb_var_screeninfo` on an 8bpp
/// lab126 panel. Recorded, not written: phase 3 reads the real value.
pub const GRAYSCALE_8BIT: u32 = 0x1;
pub const GRAYSCALE_8BIT_INVERTED: u32 = 0x2;
pub const GRAYSCALE_4BIT: u32 = 0x3;
pub const GRAYSCALE_4BIT_INVERTED: u32 = 0x4;

#[cfg(test)]
mod tests {
    use super::*;

    /// The const asserts above already gate the build; this repeats them as a
    /// runnable test so `cargo test` *reports* them rather than only failing to
    /// compile, and so the numbers appear in the test log next to their source.
    #[test]
    fn ioctl_numbers_match_koreader() {
        assert_eq!(MXCFB_SEND_UPDATE, 0x4048_462e);
        assert_eq!(MXCFB_WAIT_FOR_UPDATE_COMPLETE, 0xc008_462f);
        assert_eq!(MXCFB_WAIT_FOR_UPDATE_SUBMISSION, 0x4004_4637);
        // The size nibble of the send ioctl is the struct size, in bytes.
        assert_eq!((MXCFB_SEND_UPDATE >> 16) & 0x3fff, 72);
        assert_eq!(size_of::<MxcfbUpdateData>(), 72);
    }

    /// The one number that differs from Plato's existing Kobo V1 send ioctl —
    /// and the one that would fail silently and destructively. (The Kobo struct
    /// itself cannot be sized here: it holds a `virt_addr` pointer, so it is 68
    /// bytes only on a 32-bit target and 80 on this host.)
    #[test]
    fn send_update_differs_from_the_kobo_dialect() {
        assert_ne!(MXCFB_SEND_UPDATE, 0x4044_462e);
    }
}
