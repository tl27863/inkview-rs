//! Device details from InkView and `hwconfig.h`.
//!
//! The values come from the device's `/etc/hwconfig` (read by
//! `libhwconfig.so`, a dependency of `libinkview.so`) and from InkView's
//! version strings. The SDK headers lag the hardware: newer devices report IDs
//! that no public header names, so every enum keeps the raw value.
//!
//! Every query checks that the symbol was resolved before calling it; a field
//! is `None` when the firmware's library does not export it.

use core::ffi::{c_char, CStr};
use core::fmt;

use crate::bindings::Inkview;

/// Calls a resolved InkView symbol, or yields `None` if it is missing.
macro_rules! call {
    ($iv:expr, $name:ident $(, $arg:expr)*) => {
        $iv.$name.as_ref().ok().map(|f| unsafe { (*f)($($arg),*) })
    };
}

/// Copies a C string returned by InkView. InkView owns the memory.
fn owned_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .trim()
        .to_owned();
    (!s.is_empty()).then_some(s)
}

/// `device_id_t` from `hwconfig.h` (6.11 SDK). Unknown values are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId(pub u32);

impl DeviceId {
    /// Model number as written in `hwconfig.h` (`DEVICE_<name>`), if known.
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            5 => "631",
            17 => "641",
            18 => "627",
            19 => "614",
            20 => "615",
            21 => "740",
            22 => "616",
            23 => "632",
            24 => "626",
            25 => "840",
            26 => "840_2",
            27 => "740_2",
            28 => "1040",
            29 => "606",
            30 => "633",
            31 => "628",
            32 => "741",
            33 => "617",
            34 => "970",
            35 => "700",
            36 => "743c",
            37 => "743g",
            38 => "802",
            39 => "618",
            40 => "634",
            41 => "629",
            42 => "743k3",
            43 => "700k3",
            44 => "634k3",
            45 => "619",
            46 => "1040dl",
            47 => "700k3p",
            48 => "1030",
            // Not in any SDK header; reported by a PB710 on firmware
            // U710.6.11.2124 (GetDeviceModel() = "PB710").
            49 => "710",
            _ => return None,
        })
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name}({})", self.0),
            None => write!(f, "unknown({})", self.0),
        }
    }
}

/// `platform_id_t` (SoC family).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Mx6sl,
    A13,
    B288,
    B300,
    Rk3566,
    Unknown(u32),
}

impl Platform {
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            5 => Self::Mx6sl,
            14 => Self::A13,
            16 => Self::B288,
            17 => Self::B300,
            18 => Self::Rk3566,
            other => Self::Unknown(other),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mx6sl => f.write_str("MX6SL"),
            Self::A13 => f.write_str("A13"),
            Self::B288 => f.write_str("B288"),
            Self::B300 => f.write_str("B300"),
            Self::Rk3566 => f.write_str("RK3566"),
            Self::Unknown(raw) => write!(f, "unknown({raw})"),
        }
    }
}

/// `colormask_t`: the colour filter over the e-ink panel. Uses the 6.10+
/// numbering (`COLORMASK_CFA3` = 3 moved `COLORMASK_ACEP` from 3 to 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMask {
    None,
    Cfa,
    Cfa2,
    Cfa3,
    Acep,
    Unknown(u32),
}

impl ColorMask {
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            1 => Self::Cfa,
            2 => Self::Cfa2,
            3 => Self::Cfa3,
            4 => Self::Acep,
            other => Self::Unknown(other),
        }
    }

    /// True for any colour panel, including masks this crate does not know.
    pub fn is_color(self) -> bool {
        self != Self::None
    }
}

impl fmt::Display for ColorMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Cfa => f.write_str("CFA"),
            Self::Cfa2 => f.write_str("CFA2"),
            Self::Cfa3 => f.write_str("CFA3"),
            Self::Acep => f.write_str("ACEP"),
            Self::Unknown(raw) => write!(f, "unknown({raw})"),
        }
    }
}

/// `epdc_id_t` (e-ink display controller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epdc(pub u32);

impl Epdc {
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0 => "none",
            3 => "IMX",
            5 => "ALLWINNER_A13",
            6 => "ALLWINNER_B288",
            7 => "ALLWINNER_B300",
            8 => "T1000",
            9 => "ROCKCHIP",
            _ => return None,
        })
    }
}

impl fmt::Display for Epdc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "unknown({})", self.0),
        }
    }
}

/// Panel geometry as configured in hwconfig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// `display_id_t`, raw.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
}

/// What the firmware reports about the device. The serial number and device
/// key are deliberately not read.
#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    /// `GetDeviceModel()`, e.g. "PB700".
    pub model: Option<String>,
    /// `GetHardwareType()`.
    pub hardware: Option<String>,
    /// `GetSoftwareVersion()`, the firmware version string.
    pub firmware: Option<String>,
    pub device_id: Option<DeviceId>,
    pub platform: Option<Platform>,
    pub display: Option<DisplayInfo>,
    pub colormask: Option<ColorMask>,
    /// `native_orientation_t`: 0 landscape, 1 portrait.
    pub native_orientation: Option<u32>,
    pub epdc: Option<Epdc>,
    pub has_frontlight: Option<bool>,
    pub has_wifi: Option<bool>,
    pub has_bluetooth: Option<bool>,
    pub has_gyroscope: Option<bool>,
}

impl DeviceInfo {
    /// Queries the firmware. Never panics on missing symbols.
    pub fn query(iv: &Inkview) -> Self {
        let mut info = Self {
            model: call!(iv, GetDeviceModel).and_then(owned_string),
            hardware: call!(iv, GetHardwareType).and_then(owned_string),
            firmware: call!(iv, GetSoftwareVersion).and_then(owned_string),
            ..Self::default()
        };
        info.query_hwconfig(iv);
        info
    }

    #[cfg(not(feature = "sdk-5-19"))]
    fn query_hwconfig(&mut self, iv: &Inkview) {
        self.device_id = call!(iv, device_ID).map(DeviceId);
        self.platform = call!(iv, device_platform).map(Platform::from_raw);
        self.display = call!(iv, device_display).map(|id| DisplayInfo {
            id,
            width: call!(iv, device_display_width).unwrap_or(0),
            height: call!(iv, device_display_height).unwrap_or(0),
            dpi: call!(iv, device_display_dpi).unwrap_or(0),
        });
        self.colormask = call!(iv, device_display_colormask).map(ColorMask::from_raw);
        #[cfg(not(feature = "sdk-6-5"))]
        {
            self.native_orientation = call!(iv, device_display_native_orientation);
        }
        self.epdc = call!(iv, device_epdc).map(Epdc);
        self.has_frontlight = call!(iv, device_has_frontlight);
        self.has_wifi = call!(iv, device_has_wifi);
        self.has_bluetooth = call!(iv, device_has_bluetooth);
        self.has_gyroscope = call!(iv, device_has_gyroscope);
    }

    /// The 5.19 SDK has no hwconfig functions.
    #[cfg(feature = "sdk-5-19")]
    fn query_hwconfig(&mut self, _iv: &Inkview) {}

    /// True if the panel has a colour filter. `None` if unknown.
    pub fn is_color(&self) -> Option<bool> {
        self.colormask.map(ColorMask::is_color)
    }
}

/// One line for logs: `model=… hw=… fw=… id=… platform=… display=… …`.
impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn opt<T: fmt::Display>(v: &Option<T>) -> String {
            v.as_ref().map_or_else(|| "?".into(), ToString::to_string)
        }
        write!(
            f,
            "model={} hw={} fw={} id={} platform={} display=",
            opt(&self.model),
            opt(&self.hardware),
            opt(&self.firmware),
            opt(&self.device_id),
            opt(&self.platform),
        )?;
        match self.display {
            Some(d) => write!(f, "{}x{}@{}dpi(id {})", d.width, d.height, d.dpi, d.id)?,
            None => f.write_str("?")?,
        }
        write!(
            f,
            " colormask={} epdc={} orientation={} frontlight={} wifi={} bt={} gyro={}",
            opt(&self.colormask),
            opt(&self.epdc),
            match self.native_orientation {
                Some(0) => "landscape".into(),
                Some(1) => "portrait".into(),
                other => opt(&other),
            },
            opt(&self.has_frontlight),
            opt(&self.has_wifi),
            opt(&self.has_bluetooth),
            opt(&self.has_gyroscope),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_keep_unknown_values() {
        assert_eq!(DeviceId(35).name(), Some("700"));
        assert_eq!(DeviceId(48).name(), Some("1030"));
        assert_eq!(DeviceId(48).to_string(), "1030(48)");
        assert_eq!(DeviceId(49).to_string(), "710(49)");
        assert_eq!(DeviceId(50).name(), None);
        assert_eq!(DeviceId(50).to_string(), "unknown(50)");
    }

    #[test]
    fn platforms_and_colormasks_map_from_raw() {
        assert_eq!(Platform::from_raw(16), Platform::B288);
        assert_eq!(Platform::from_raw(18), Platform::Rk3566);
        assert_eq!(Platform::from_raw(99), Platform::Unknown(99));
        assert_eq!(ColorMask::from_raw(4), ColorMask::Acep);
        assert!(!ColorMask::from_raw(0).is_color());
        assert!(ColorMask::from_raw(3).is_color());
        assert!(ColorMask::from_raw(7).is_color());
    }

    #[test]
    fn display_line_marks_missing_fields() {
        let info = DeviceInfo {
            model: Some("PB700".into()),
            device_id: Some(DeviceId(35)),
            platform: Some(Platform::B288),
            display: Some(DisplayInfo {
                id: 13,
                width: 1264,
                height: 1680,
                dpi: 300,
            }),
            colormask: Some(ColorMask::None),
            native_orientation: Some(1),
            ..DeviceInfo::default()
        };
        let line = info.to_string();
        assert!(line.starts_with("model=PB700 hw=? fw=? id=700(35) platform=B288"));
        assert!(line.contains("display=1264x1680@300dpi(id 13)"));
        assert!(line.contains("colormask=none"));
        assert!(line.contains("orientation=portrait"));
    }
}
