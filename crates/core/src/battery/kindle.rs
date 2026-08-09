//! `KindleBattery` — `/sys/class/power_supply/*/{capacity,status}`.
//!
//! Same two files as `KoboBattery`, found by globbing rather than by a fixed
//! list, because the node name on this hardware is a *Likely*, not a
//! *Confirmed*: ezkindle's `just battery` reads `/sys/class/power_supply/*/capacity`
//! on the real device and does not record which node answered.
//!
//! The wario tree KOReader encodes for the PW3 is written down below as the
//! documented fallback. Phase 3 (`PLATO-DEVICE-PROBES`) replaces the guesswork
//! with an `ls`.

use std::fs::{self, File};
use std::path::PathBuf;
use std::io::{Read, Seek, SeekFrom};
use anyhow::{Error, format_err};
use super::{Battery, Status};

/// Where to look, in order. The glob comes first because it cannot be wrong;
/// the named paths only matter if a node exposes `capacity` without being
/// enumerable, which is not expected.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// Documented fallbacks, from KOReader's Kindle frontend (`device/kindle`) and
/// ezkindle `docs/device.md`. Not currently reachable — the glob subsumes the
/// first two — and kept here so the next session does not have to re-derive
/// them if the glob comes up empty on device:
///
/// * `/sys/class/power_supply/max77696-battery/capacity` — the PMIC's own node;
///   `max77696` is Confirmed as the PW3's PMIC.
/// * `/sys/class/power_supply/bd71827_bat` — Kobo's, listed for contrast.
/// * `/sys/devices/system/wario_battery/wario_battery0/battery_capacity` — the
///   wario platform tree, which is what KOReader actually reads on this
///   generation. It reports a bare integer percentage, *not* a `power_supply`
///   node, so it needs `capacity`-only handling and no `status` file; if the
///   glob fails on device, this is the shape to add.
#[allow(dead_code)]
const WARIO_CAPACITY: &str = "/sys/devices/system/wario_battery/wario_battery0/battery_capacity";

const BATTERY_CAPACITY: &str = "capacity";
const BATTERY_STATUS: &str = "status";

pub struct KindleBattery {
    capacity: File,
    status: Option<File>,
}

impl KindleBattery {
    pub fn new() -> Result<KindleBattery, Error> {
        let base = Self::find_supply()
            .ok_or_else(|| format_err!("no battery under {} exposes {} \
                                        (and {} is absent)",
                                       POWER_SUPPLY_ROOT, BATTERY_CAPACITY,
                                       WARIO_CAPACITY))?;
        let capacity = File::open(base.join(BATTERY_CAPACITY))?;
        // `status` is optional on purpose: a device that can report its charge
        // but not its charging state should still show a battery, not fail to
        // start. Missing `status` reads as Unknown.
        let status = File::open(base.join(BATTERY_STATUS)).ok();
        Ok(KindleBattery { capacity, status })
    }

    /// The first entry under `/sys/class/power_supply` that has a `capacity`
    /// file. Entries are sorted so the choice is deterministic across boots —
    /// `read_dir` order is not.
    fn find_supply() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = fs::read_dir(POWER_SUPPLY_ROOT)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join(BATTERY_CAPACITY).exists())
            .collect();
        candidates.sort();
        candidates.into_iter().next()
    }
}

fn read_trimmed(file: &mut File) -> Result<String, Error> {
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut buf)?;
    Ok(buf.trim_end().to_string())
}

impl Battery for KindleBattery {
    fn capacity(&mut self) -> Result<Vec<f32>, Error> {
        let capacity = read_trimmed(&mut self.capacity)?
            .parse::<f32>()
            .unwrap_or(0.0);
        Ok(vec![capacity])
    }

    fn status(&mut self) -> Result<Vec<Status>, Error> {
        let status = match self.status.as_mut() {
            Some(file) => match read_trimmed(file)?.as_str() {
                "Discharging" => Status::Discharging,
                "Charging" => Status::Charging,
                "Not charging" | "Full" => Status::Charged,
                _ => Status::Unknown,
            },
            None => Status::Unknown,
        };
        Ok(vec![status])
    }
}
