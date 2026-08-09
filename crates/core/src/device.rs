use std::env;
use std::fmt;
use lazy_static::lazy_static;
use crate::input::TouchProto;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Model {
    /// Not a Kobo. See `Device::new` for how this is selected, and PATCHES.md
    /// for the audit of every `Device` method that has to answer for it.
    KindlePaperwhite3,
    LibraColour,
    ClaraColour,
    ClaraBW,
    Elipsa2E,
    Clara2E,
    Libra2,
    Sage,
    Elipsa,
    Nia,
    LibraH2O,
    Forma32GB,
    Forma,
    ClaraHD,
    AuraH2OEd2V2,
    AuraH2OEd2V1,
    AuraEd2V2,
    AuraEd2V1,
    AuraONELimEd,
    AuraONE,
    Touch2,
    GloHD,
    AuraH2O,
    Aura,
    AuraHD,
    Mini,
    Glo,
    TouchC,
    TouchAB,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Orientation {
    Portrait,
    Landscape,
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Model::KindlePaperwhite3 => write!(f, "Kindle Paperwhite 3"),
            Model::LibraColour   => write!(f, "Libra Colour"),
            Model::ClaraColour   => write!(f, "Clara Colour"),
            Model::ClaraBW       => write!(f, "Clara BW"),
            Model::Elipsa2E      => write!(f, "Elipsa 2E"),
            Model::Clara2E       => write!(f, "Clara 2E"),
            Model::Libra2        => write!(f, "Libra 2"),
            Model::Sage          => write!(f, "Sage"),
            Model::Elipsa        => write!(f, "Elipsa"),
            Model::Nia           => write!(f, "Nia"),
            Model::LibraH2O      => write!(f, "Libra H₂O"),
            Model::Forma32GB     => write!(f, "Forma 32GB"),
            Model::Forma         => write!(f, "Forma"),
            Model::ClaraHD       => write!(f, "Clara HD"),
            Model::AuraH2OEd2V1  => write!(f, "Aura H₂O Edition 2 Version 1"),
            Model::AuraH2OEd2V2  => write!(f, "Aura H₂O Edition 2 Version 2"),
            Model::AuraEd2V1     => write!(f, "Aura Edition 2 Version 1"),
            Model::AuraEd2V2     => write!(f, "Aura Edition 2 Version 2"),
            Model::AuraONELimEd  => write!(f, "Aura ONE Limited Edition"),
            Model::AuraONE       => write!(f, "Aura ONE"),
            Model::Touch2        => write!(f, "Touch 2.0"),
            Model::GloHD         => write!(f, "Glo HD"),
            Model::AuraH2O       => write!(f, "Aura H₂O"),
            Model::Aura          => write!(f, "Aura"),
            Model::AuraHD        => write!(f, "Aura HD"),
            Model::Mini          => write!(f, "Mini"),
            Model::Glo           => write!(f, "Glo"),
            Model::TouchC        => write!(f, "Touch C"),
            Model::TouchAB       => write!(f, "Touch A/B"),
        }
    }
}

#[derive(Debug)]
pub struct Device {
    pub model: Model,
    pub proto: TouchProto,
    pub dims: (u32, u32),
    pub dpi: u16,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum FrontlightKind {
    Standard,
    Natural,
    Premixed,
}

/// The value of `PLATO_DEVICE` that selects the Kindle backend.
///
/// Detection is an **explicit opt-in**, checked before `PRODUCT`, and it is
/// deliberately not a sniff of the running system. A Kobo exports `PRODUCT`
/// from its own init; a Kindle exports nothing, so any heuristic ("does
/// /dev/ntx_io exist", "is there an mxcfb") would be a guess that, if it ever
/// misfired on a Kobo, would send Kobo hardware a 72-byte update struct. An env
/// var cannot misfire, and it costs one line in the launcher script.
pub const KINDLE_PW3_DEVICE: &str = "kindle-pw3";

impl Device {
    pub fn new(product: &str, model_number: &str) -> Device {
        Device::detect(&env::var("PLATO_DEVICE").unwrap_or_default(), product, model_number)
    }

    pub fn detect(plato_device: &str, product: &str, model_number: &str) -> Device {
        if plato_device == KINDLE_PW3_DEVICE {
            return Device {
                model: Model::KindlePaperwhite3,
                // cyttsp4_mt on /dev/input/event1, protocol B tracked by slot.
                // Confirmed on the device: the ABS bitmap is
                // SLOT + POSITION_X + POSITION_Y + TRACKING_ID and nothing
                // else, so none of the pressure-keyed modes can see a finger
                // here. Coordinates are identity-mapped screen pixels, which
                // is what the startup_rotation() choice below preserves.
                proto: TouchProto::MultiSlot,
                dims: (1072, 1448),
                dpi: 300,
            };
        }
        match product {
            "kraken" => Device {
                model: Model::Glo,
                proto: TouchProto::Single,
                dims: (758, 1024),
                dpi: 212,
            },
            "pixie" => Device {
                model: Model::Mini,
                proto: TouchProto::Single,
                dims: (600, 800),
                dpi: 200,
            },
            "dragon" => Device {
                model: Model::AuraHD,
                proto: TouchProto::Single,
                dims: (1080, 1440),
                dpi: 265,
            },
            "phoenix" => Device {
                model: Model::Aura,
                proto: TouchProto::MultiA,
                dims: (758, 1024),
                dpi: 212,
            },
            "dahlia" => Device {
                model: Model::AuraH2O,
                proto: TouchProto::MultiA,
                dims: (1080, 1440),
                dpi: 265,
            },
            "alyssum" => Device {
                model: Model::GloHD,
                proto: TouchProto::MultiA,
                dims: (1072, 1448),
                dpi: 300,
            },
            "pika" => Device {
                model: Model::Touch2,
                proto: TouchProto::MultiA,
                dims: (600, 800),
                dpi: 167,
            },
            "daylight" => Device {
                model: if model_number == "381" { Model::AuraONELimEd } else { Model::AuraONE },
                proto: TouchProto::MultiA,
                dims: (1404, 1872),
                dpi: 300,
            },
            "star" => Device {
                model: if model_number == "379" { Model::AuraEd2V2 } else { Model::AuraEd2V1 },
                proto: TouchProto::MultiA,
                dims: (758, 1024),
                dpi: 212,
            },
            "snow" => Device {
                model: if model_number == "378" { Model::AuraH2OEd2V2 } else { Model::AuraH2OEd2V1 },
                proto: TouchProto::MultiB,
                dims: (1080, 1440),
                dpi: 265,
            },
            "nova" => Device {
                model: Model::ClaraHD,
                proto: TouchProto::MultiB,
                dims: (1072, 1448),
                dpi: 300,
            },
            "frost" => Device {
                model: if model_number == "380" { Model::Forma32GB } else { Model::Forma },
                proto: TouchProto::MultiB,
                dims: (1440, 1920),
                dpi: 300,
            },
            "storm" => Device {
                model: Model::LibraH2O,
                proto: TouchProto::MultiB,
                dims: (1264, 1680),
                dpi: 300,
            },
            "luna" => Device {
                model: Model::Nia,
                proto: TouchProto::MultiA,
                dims: (758, 1024),
                dpi: 212,
            },
            "europa" => Device {
                model: Model::Elipsa,
                proto: TouchProto::MultiC,
                dims: (1404, 1872),
                dpi: 227,
            },
            "cadmus" => Device {
                model: Model::Sage,
                proto: TouchProto::MultiC,
                dims: (1440, 1920),
                dpi: 300,
            },
            "io" => Device {
                model: Model::Libra2,
                proto: TouchProto::MultiC,
                dims: (1264, 1680),
                dpi: 300,
            },
            "goldfinch" => Device {
                model: Model::Clara2E,
                proto: TouchProto::MultiB,
                dims: (1072, 1448),
                dpi: 300,
            },
            "condor" => Device {
                model: Model::Elipsa2E,
                proto: TouchProto::MultiC,
                dims: (1404, 1872),
                dpi: 227,
            },
            "spaBW" | "spaBWTPV" => Device {
                model: Model::ClaraBW,
                proto: TouchProto::MultiB,
                dims: (1072, 1448),
                dpi: 300,
            },
            "spaColour" => Device {
                model: Model::ClaraColour,
                proto: TouchProto::MultiB,
                dims: (1072, 1448),
                dpi: 300,
            },
            "monza" => Device {
                model: Model::LibraColour,
                proto: TouchProto::MultiB,
                dims: (1264, 1680),
                dpi: 300,
            },
            _ => Device {
                model: if model_number == "320" { Model::TouchC } else { Model::TouchAB },
                proto: TouchProto::Single,
                dims: (600, 800),
                dpi: 167,
            },
        }
    }

    pub fn is_kindle(&self) -> bool {
        matches!(self.model, Model::KindlePaperwhite3)
    }

    pub fn color_samples(&self) -> usize {
        match self.model {
            Model::ClaraColour | Model::LibraColour => 3,
            _ => 1,
        }
    }

    pub fn frontlight_kind(&self) -> FrontlightKind {
        match self.model {
            Model::ClaraHD |
            Model::Forma |
            Model::Forma32GB |
            Model::LibraH2O |
            Model::Sage |
            Model::Libra2 |
            Model::Clara2E |
            Model::Elipsa2E |
            Model::ClaraBW |
            Model::ClaraColour |
            Model::LibraColour => FrontlightKind::Premixed,
            Model::AuraONE |
            Model::AuraONELimEd |
            Model::AuraH2OEd2V1 |
            Model::AuraH2OEd2V2 => FrontlightKind::Natural,
            _ => FrontlightKind::Standard,
        }
    }

    pub fn has_natural_light(&self) -> bool {
        self.frontlight_kind() != FrontlightKind::Standard
    }

    pub fn has_lightsensor(&self) -> bool {
        matches!(self.model,
                 Model::AuraONE | Model::AuraONELimEd)
    }

    pub fn has_gyroscope(&self) -> bool {
        matches!(self.model,
                 Model::Forma | Model::Forma32GB | Model::LibraH2O | Model::Elipsa |
                 Model::Sage | Model::Libra2 | Model::Elipsa2E | Model::LibraColour)
    }

    pub fn has_page_turn_buttons(&self) -> bool {
        matches!(self.model,
                 Model::Forma | Model::Forma32GB | Model::LibraH2O |
                 Model::Sage | Model::Libra2 | Model::LibraColour)
    }

    pub fn has_power_cover(&self) -> bool {
        matches!(self.model, Model::Sage)
    }

    pub fn has_removable_storage(&self) -> bool {
        matches!(self.model,
                 Model::AuraH2O | Model::Aura | Model::AuraHD |
                 Model::Glo | Model::TouchAB | Model::TouchC)
    }

    pub fn should_invert_buttons(&self, rotation: i8) -> bool {
        let sr = self.startup_rotation();
        let (_, dir) = self.mirroring_scheme();

        rotation == (4 + sr - dir) % 4 || rotation == (4 + sr - 2 * dir) % 4
    }

    pub fn orientation(&self, rotation: i8) -> Orientation {
        if self.should_swap_axes(rotation) {
            Orientation::Portrait
        } else {
            Orientation::Landscape
        }
    }

    /// The Kobo generation ladder. A Kindle does not sit on it, so
    /// `KindlePaperwhite3` answers **6** — the value that makes every site
    /// outside `kobo1.rs`/`kobo2.rs` behave like a mark <= 6 Kobo, which is the
    /// Carta/GloHD-era generation the PW3 is contemporary with. Every consulted
    /// site is listed in PATCHES.md; none of them is reached with this model
    /// except the cosmetic "Mark N" row in the system-info page.
    pub fn mark(&self) -> u8 {
        match self.model {
            Model::KindlePaperwhite3 => 6,
            Model::LibraColour => 13,
            Model::ClaraBW |
            Model::ClaraColour => 12,
            Model::Elipsa2E => 11,
            Model::Clara2E => 10,
            Model::Libra2 => 9,
            Model::Sage |
            Model::Elipsa => 8,
            Model::Nia |
            Model::LibraH2O |
            Model::Forma32GB |
            Model::Forma |
            Model::ClaraHD |
            Model::AuraH2OEd2V2 |
            Model::AuraEd2V2 => 7,
            Model::AuraH2OEd2V1 |
            Model::AuraEd2V1 |
            Model::AuraONELimEd |
            Model::AuraONE |
            Model::Touch2 |
            Model::GloHD => 6,
            Model::AuraH2O |
            Model::Aura => 5,
            Model::AuraHD |
            Model::Mini |
            Model::Glo |
            Model::TouchC => 4,
            Model::TouchAB => 3,
        }
    }

    pub fn should_mirror_axes(&self, rotation: i8) -> (bool, bool) {
        let (mxy, dir) = self.mirroring_scheme();
        let mx = (4 + (mxy + dir)) % 4;
        let my = (4 + (mxy - dir)) % 4;
        let mirror_x = mxy == rotation || mx == rotation;
        let mirror_y = mxy == rotation || my == rotation;
        (mirror_x, mirror_y)
    }

    // Returns the center and direction of the mirroring pattern.
    pub fn mirroring_scheme(&self) -> (i8, i8) {
        match self.model {
            Model::AuraH2OEd2V1 |
            Model::LibraH2O |
            Model::Libra2 => (3, 1),
            Model::Sage => (0, 1),
            Model::AuraH2OEd2V2 => (0, -1),
            Model::Forma | Model::Forma32GB => (2, -1),
            _ => (2, 1),
        }
    }

    pub fn should_swap_axes(&self, rotation: i8) -> bool {
        rotation % 2 == self.swapping_scheme()
    }

    pub fn swapping_scheme(&self) -> i8 {
        match self.model {
            Model::LibraH2O => 0,
            _ => 1,
        }
    }

    // The written rotation that makes the screen be in portrait mode
    // with the Kobo logo at the bottom.
    pub fn startup_rotation(&self) -> i8 {
        match self.model {
            // The panel's only rotation; KindleFramebuffer refuses any other.
            //
            // 0 with the *default* swapping (1) and mirroring ((2, 1)) schemes
            // is what makes the touch transform the identity, which is what
            // KOReader records for the PW3: protocol B on /dev/input/event1,
            // no coordinate transform. should_swap_axes(0) is false, so
            // input.rs leaves ABS_MT_POSITION_X/Y alone, and
            // should_mirror_axes(0) is (false, false).
            //
            // The cost is that orientation(0) reads as Landscape rather than
            // Portrait, because Plato's model ties "portrait" to "the touch
            // panel is landscape-native" -- true of every Kobo, false here. It
            // is unobservable on this device: all three consumers of
            // orientation() (app.rs:549, :744, :851) either need a gyroscope,
            // which this model does not have, or merely guard a set_rotation()
            // call that KindleFramebuffer refuses anyway. Touch correctness is
            // the thing that would actually break, so it wins.
            Model::KindlePaperwhite3 => 0,
            Model::LibraH2O => 0,
            Model::AuraH2OEd2V1 |
            Model::Forma | Model::Forma32GB |
            Model::Sage | Model::Libra2 | Model::Elipsa2E |
            Model::LibraColour => 1,
            _ => 3,
        }
    }

    // Return a device independent rotation value given
    // the device dependent written rotation value *n*.
    pub fn to_canonical(&self, n: i8) -> i8 {
        let (_, dir) = self.mirroring_scheme();
        (4 + dir * (n - self.startup_rotation())) % 4
    }

    // Return a device dependent written rotation value given
    // the device independent rotation value *n*.
    pub fn from_canonical(&self, n: i8) -> i8 {
        let (_, dir) = self.mirroring_scheme();
        (self.startup_rotation() + (4 + dir * n) % 4) % 4
    }

    // Return a device dependent written rotation value given
    // the device dependent read rotation value *n*.
    pub fn transformed_rotation(&self, n: i8) -> i8 {
        match self.model {
            Model::AuraHD | Model::AuraH2O => n ^ 2,
            Model::AuraH2OEd2V2 |
            Model::Forma | Model::Forma32GB => (4 - n) % 4,
            _ => n,
        }
    }

    pub fn transformed_gyroscope_rotation(&self, n: i8) -> i8 {
        match self.model {
            Model::LibraH2O => n ^ 1,
            Model::Libra2 |
            Model::Sage |
            Model::Elipsa2E |
            Model::LibraColour => (6 - n) % 4,
            Model::Elipsa => (4 - n) % 4,
            _ => n,
        }
    }
}

lazy_static! {
    pub static ref CURRENT_DEVICE: Device = {
        let product = env::var("PRODUCT").unwrap_or_default();
        let model_number = env::var("MODEL_NUMBER").unwrap_or_default();

        Device::new(&product, &model_number)
    };
}

#[cfg(test)]
mod tests {
    use super::{Device, Model, Orientation, FrontlightKind, KINDLE_PW3_DEVICE};
    use crate::input::TouchProto;

    #[test]
    fn test_kindle_env_var_beats_product() {
        // A Kobo PRODUCT alongside the Kindle opt-in: the opt-in wins, because
        // it is checked before the match, so no Kobo string can shadow it.
        let d = Device::detect(KINDLE_PW3_DEVICE, "alyssum", "371");
        assert_eq!(d.model, Model::KindlePaperwhite3);
        assert_eq!(d.dims, (1072, 1448));
        assert_eq!(d.dpi, 300);
        assert_eq!(d.proto, TouchProto::MultiSlot);
        assert!(d.is_kindle());
    }

    /// `TouchProto::MultiSlot` is new, and the one way it could hurt a Kobo is
    /// by being wired to one. Pin every product's protocol.
    #[test]
    fn test_no_kobo_touch_protocol_changed() {
        let expected = [
            ("kraken", TouchProto::Single), ("pixie", TouchProto::Single),
            ("dragon", TouchProto::Single), ("phoenix", TouchProto::MultiA),
            ("dahlia", TouchProto::MultiA), ("alyssum", TouchProto::MultiA),
            ("pika", TouchProto::MultiA), ("daylight", TouchProto::MultiA),
            ("star", TouchProto::MultiA), ("snow", TouchProto::MultiB),
            ("nova", TouchProto::MultiB), ("frost", TouchProto::MultiB),
            ("storm", TouchProto::MultiB), ("luna", TouchProto::MultiA),
            ("europa", TouchProto::MultiC), ("cadmus", TouchProto::MultiC),
            ("io", TouchProto::MultiC), ("goldfinch", TouchProto::MultiB),
            ("spaBW", TouchProto::MultiB), ("spaBWTPV", TouchProto::MultiB),
            ("spaColour", TouchProto::MultiB),
            ("monza", TouchProto::MultiB), ("condor", TouchProto::MultiC),
            ("", TouchProto::Single),
        ];
        for (product, proto) in expected {
            let d = Device::detect("", product, "");
            assert_eq!(d.proto, proto, "PRODUCT={product:?}");
            assert!(!d.is_kindle());
        }
    }

    #[test]
    fn test_kindle_detection_never_misfires_on_kobo() {
        for plato_device in ["", "kobo", "kindle", "kindle-pw4", "KINDLE-PW3"] {
            let d = Device::detect(plato_device, "alyssum", "371");
            assert_eq!(d.model, Model::GloHD, "PLATO_DEVICE={plato_device:?}");
            assert!(!d.is_kindle());
        }
        // An unset PLATO_DEVICE with an unknown PRODUCT still falls through to
        // the Kobo default, exactly as before.
        assert_eq!(Device::detect("", "", "").model, Model::TouchAB);
    }

    #[test]
    fn test_kindle_geometry_and_capabilities() {
        let d = Device::detect(KINDLE_PW3_DEVICE, "", "");
        // Portrait at the panel's only rotation, with no touch axis transform.
        assert_eq!(d.startup_rotation(), 0);
        // The identity touch transform: no axis swap, no mirroring. This is
        // the assertion that matters -- see startup_rotation() for why
        // orientation() reads Landscape here and why that is harmless.
        assert!(!d.should_swap_axes(0));
        assert_eq!(d.should_mirror_axes(0), (false, false));
        assert_eq!(d.orientation(0), Orientation::Landscape);
        assert_eq!(d.to_canonical(0), 0);
        assert_eq!(d.from_canonical(0), 0);
        assert_eq!(d.transformed_rotation(0), 0);
        // The capability answers everything outside kobo1.rs routes on.
        assert_eq!(d.mark(), 6);
        assert_eq!(d.color_samples(), 1);
        assert_eq!(d.frontlight_kind(), FrontlightKind::Standard);
        assert!(!d.has_natural_light());
        assert!(!d.has_lightsensor());
        assert!(!d.has_gyroscope());
        assert!(!d.has_page_turn_buttons());
        assert!(!d.has_power_cover());
        assert!(!d.has_removable_storage());
    }

    #[test]
    fn test_device_canonical_rotation() {
        let forma = Device::new("frost", "377");
        let aura_one = Device::new("daylight", "373");
        for n in 0..4 {
            assert_eq!(forma.from_canonical(forma.to_canonical(n)), n);
        }
        assert_eq!(aura_one.from_canonical(0), aura_one.startup_rotation());
        assert_eq!(forma.from_canonical(1) - forma.from_canonical(0),
                   aura_one.from_canonical(2) - aura_one.from_canonical(3));
    }
}
