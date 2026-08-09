//! `KindleFrontlight` — `/sys/class/backlight/max77696-bl/brightness`.
//!
//! `max77696` is the PW3's PMIC (Confirmed, ezkindle `docs/device.md`); the
//! backlight node name is what KOReader encodes for this generation (Likely
//! until `PLATO-DEVICE-PROBES` runs an `ls`).
//!
//! Plato's `Frontlight` trait speaks percentages, the sysfs node speaks raw
//! steps in `0 ..= max_brightness`, so this is a scaling wrapper and nothing
//! else. `max_brightness` is read once at construction rather than assumed:
//! Amazon's own value on this generation is reported as 24 in some places and
//! 25 in others, and a hardcoded ceiling would either clip the top of the range
//! or write a value the driver rejects.
//!
//! Worth writing down even though it does not apply here: driving the frontlight
//! **over lipc** (`lipc-set-prop com.lab126.powerd flIntensity`) has the quirk
//! that 0 is not off — powerd keeps a floor and the light stays faintly on.
//! Writing sysfs directly, as we do, bypasses powerd entirely, so 0 really is 0.
//! If a future revision ever moves to lipc (e.g. to keep powerd's own idea of
//! the level in sync), that quirk comes back.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use anyhow::{Error, Context};
use super::{Frontlight, LightLevels};

const FRONTLIGHT_INTERFACE: &str = "/sys/class/backlight/max77696-bl";
const BRIGHTNESS: &str = "brightness";
const MAX_BRIGHTNESS: &str = "max_brightness";

/// Used only if `max_brightness` is unreadable — better a working light with a
/// slightly wrong ceiling than no light at all.
const FALLBACK_MAX_BRIGHTNESS: u32 = 24;

pub struct KindleFrontlight {
    value: f32,
    max: u32,
    interface: File,
}

/// Percentage (0..=100, clamped) to a raw step in `0..=max`.
///
/// Split out and tested because it is pure arithmetic that is annoying to
/// verify on a device: the rounding has to make 100% reach `max` exactly, and
/// 0% reach 0 exactly, on any `max`.
pub fn scale_intensity(value: f32, max: u32) -> u32 {
    let pct = value.clamp(0.0, 100.0);
    ((pct / 100.0) * max as f32).round() as u32
}

/// The inverse, for reporting a level read back off the device.
#[allow(dead_code)]
pub fn unscale_intensity(raw: u32, max: u32) -> f32 {
    if max == 0 {
        return 0.0;
    }
    100.0 * raw.min(max) as f32 / max as f32
}

impl KindleFrontlight {
    pub fn new(value: f32) -> Result<KindleFrontlight, Error> {
        let base = Path::new(FRONTLIGHT_INTERFACE);
        let max = read_u32(&base.join(MAX_BRIGHTNESS)).unwrap_or_else(|e| {
            eprintln!("Can't read {}: {:#}; assuming {}.",
                      base.join(MAX_BRIGHTNESS).display(), e, FALLBACK_MAX_BRIGHTNESS);
            FALLBACK_MAX_BRIGHTNESS
        });
        let interface = OpenOptions::new().write(true)
                                          .open(base.join(BRIGHTNESS))
                                          .with_context(|| format!("can't open {}",
                                                                   base.join(BRIGHTNESS).display()))?;
        Ok(KindleFrontlight { value, max, interface })
    }

    pub fn max_brightness(&self) -> u32 {
        self.max
    }
}

fn read_u32(path: &Path) -> Result<u32, Error> {
    let mut file = File::open(path)?;
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut buf)?;
    Ok(buf.trim_end().parse::<u32>()?)
}

impl Frontlight for KindleFrontlight {
    fn set_intensity(&mut self, value: f32) {
        let raw = scale_intensity(value, self.max);
        let written = self.interface.write_all(format!("{}\n", raw).as_bytes())
                          .and_then(|_| self.interface.flush());
        if written.is_ok() {
            self.value = value;
        } else if let Err(e) = written {
            eprintln!("Can't set frontlight intensity: {:#}.", e);
        }
    }

    /// The PW3's frontlight is a single white channel — no warmth to set.
    fn set_warmth(&mut self, _value: f32) { }

    fn levels(&self) -> LightLevels {
        LightLevels {
            intensity: self.value,
            warmth: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_hits_both_ends_exactly() {
        for max in [1, 12, 24, 25, 100, 255] {
            assert_eq!(scale_intensity(0.0, max), 0, "max={max}");
            assert_eq!(scale_intensity(100.0, max), max, "max={max}");
            assert_eq!(scale_intensity(50.0, 24), 12);
        }
    }

    #[test]
    fn scaling_clamps_rather_than_wrapping() {
        assert_eq!(scale_intensity(-40.0, 24), 0);
        assert_eq!(scale_intensity(1000.0, 24), 24);
        // f32 -> u32 `as` casts saturate in Rust, but the clamp means we never
        // rely on that; check the pre-clamp negative case explicitly.
        assert_eq!(scale_intensity(f32::NAN.max(0.0), 24), 0);
    }

    #[test]
    fn scaling_is_monotone_and_round_trips_every_step() {
        for max in [12, 24, 25] {
            let mut previous = 0;
            for pct in 0..=100 {
                let raw = scale_intensity(pct as f32, max);
                assert!(raw >= previous, "max={max} pct={pct}");
                assert!(raw <= max);
                previous = raw;
            }
            // Every raw step is reachable from some percentage.
            for raw in 0..=max {
                let pct = unscale_intensity(raw, max);
                assert_eq!(scale_intensity(pct, max), raw, "max={max} raw={raw}");
            }
        }
    }

    #[test]
    fn unscaling_survives_a_degenerate_max() {
        assert_eq!(unscale_intensity(0, 0), 0.0);
        assert_eq!(unscale_intensity(9, 0), 0.0);
        assert_eq!(unscale_intensity(99, 24), 100.0);
    }
}
