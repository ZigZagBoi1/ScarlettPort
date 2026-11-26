//! Controller detection for the Play page: Xbox pads through XInput, PlayStation pads
//! (DualShock 4, DualSense, DualSense Edge) through the raw-input HID list. The runtime reads
//! PlayStation pads itself over USB when `SCARLETT_PAD_PS=1` (set by every World preset).
//! Polled at most every two seconds; the naming and the line are pure and unit tested.
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detected {
    /// Connected XInput slots (0..=4).
    pub xbox: u32,
    /// PlayStation pads by name.
    pub ps: Vec<&'static str>,
}

pub const SONY: u32 = 0x054C;

/// The name of a Sony HID product the runtime drives, or None.
pub fn sony_name(pid: u32) -> Option<&'static str> {
    match pid {
        0x05C4 | 0x09CC => Some("DualShock 4"),
        0x0BA0 => Some("DualShock 4 (wireless adapter)"),
        0x0CE6 => Some("DualSense"),
        0x0DF2 => Some("DualSense Edge"),
        _ => None,
    }
}

/// (detail, status) for the Play page's Controller row.
pub fn line(d: &Detected) -> (String, &'static str) {
    let mut parts = Vec::new();
    match d.xbox {
        0 => {}
        1 => parts.push("Xbox controller".to_string()),
        n => parts.push(format!("{n} Xbox controllers")),
    }
    for n in &d.ps {
        parts.push(format!("{n} (USB, read natively)"));
    }
    if parts.is_empty() {
        return ("No pad found: plug in an Xbox or PlayStation pad (USB), or use keyboard and mouse".into(), "None");
    }
    let status = match (d.xbox > 0, !d.ps.is_empty()) {
        (true, true) => "Xbox + PS",
        (true, false) => "Xbox",
        _ => "PlayStation",
    };
    (parts.join("  ·  "), status)
}

static CACHE: Mutex<Option<(Instant, Detected)>> = Mutex::new(None);

/// The pads connected now (cached for two seconds).
pub fn detected() -> Detected {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, d)) = c.as_ref() {
        if t.elapsed() < Duration::from_secs(2) {
            return d.clone();
        }
    }
    let d = Detected { xbox: sys::xbox(), ps: sys::sony().into_iter().filter_map(sony_name).collect() };
    *c = Some((Instant::now(), d.clone()));
    d
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    #[repr(C)]
    struct XInputState {
        packet: u32,
        buttons: u16,
        triggers: [u8; 2],
        thumbs: [i16; 4],
    }

    #[repr(C)]
    struct RawInputDeviceList {
        device: *mut c_void,
        kind: u32,
    }

    #[repr(C)]
    struct HidInfo {
        vendor: u32,
        product: u32,
        version: u32,
        usage_page: u16,
        usage: u16,
    }

    #[repr(C)]
    struct DeviceInfo {
        size: u32,
        kind: u32,
        hid: HidInfo,
        _pad: [u32; 2],
    }

    #[link(name = "xinput")]
    extern "system" {
        fn XInputGetState(user: u32, state: *mut XInputState) -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetRawInputDeviceList(list: *mut RawInputDeviceList, count: *mut u32, size: u32) -> u32;
        fn GetRawInputDeviceInfoW(device: *mut c_void, command: u32, data: *mut c_void, size: *mut u32) -> u32;
    }

    pub fn xbox() -> u32 {
        (0..4)
            .filter(|&i| {
                // SAFETY: a zeroed XINPUT_STATE the call writes into.
                let mut s: XInputState = unsafe { std::mem::zeroed() };
                unsafe { XInputGetState(i, &mut s) == 0 }
            })
            .count() as u32
    }

    /// Product ids of Sony game pads (HID usage page 1, joystick or gamepad).
    pub fn sony() -> Vec<u32> {
        const RIM_TYPEHID: u32 = 2;
        const RIDI_DEVICEINFO: u32 = 0x2000_000B;
        let size = std::mem::size_of::<RawInputDeviceList>() as u32;
        let mut n = 0u32;
        // SAFETY: a null list asks for the count only.
        if unsafe { GetRawInputDeviceList(std::ptr::null_mut(), &mut n, size) } != 0 || n == 0 {
            return Vec::new();
        }
        let mut list: Vec<RawInputDeviceList> = (0..n).map(|_| RawInputDeviceList { device: std::ptr::null_mut(), kind: 0 }).collect();
        // SAFETY: `list` holds `n` entries of the size passed.
        let got = unsafe { GetRawInputDeviceList(list.as_mut_ptr(), &mut n, size) };
        if got == u32::MAX {
            return Vec::new();
        }
        list.truncate(got as usize);
        let mut out = Vec::new();
        for d in list.iter().filter(|d| d.kind == RIM_TYPEHID) {
            // SAFETY: a zeroed RID_DEVICE_INFO with cbSize set, and its size.
            let mut info: DeviceInfo = unsafe { std::mem::zeroed() };
            info.size = std::mem::size_of::<DeviceInfo>() as u32;
            let mut sz = info.size;
            let r = unsafe { GetRawInputDeviceInfoW(d.device, RIDI_DEVICEINFO, &mut info as *mut _ as *mut c_void, &mut sz) };
            if r == u32::MAX || info.kind != RIM_TYPEHID {
                continue;
            }
            let h = &info.hid;
            if h.vendor == super::SONY && h.usage_page == 1 && (h.usage == 4 || h.usage == 5) && !out.contains(&h.product) {
                out.push(h.product);
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn xbox() -> u32 {
        0
    }
    pub fn sony() -> Vec<u32> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sony_pads() {
        assert_eq!(sony_name(0x0CE6), Some("DualSense"));
        assert_eq!(sony_name(0x0DF2), Some("DualSense Edge"));
        assert_eq!(sony_name(0x09CC), Some("DualShock 4"));
        assert_eq!(sony_name(0x1234), None);
    }

    #[test]
    fn line_names_what_is_there() {
        assert_eq!(line(&Detected::default()).1, "None");
        assert_eq!(line(&Detected { xbox: 1, ps: vec![] }), ("Xbox controller".to_string(), "Xbox"));
        let (m, s) = line(&Detected { xbox: 0, ps: vec!["DualSense"] });
        assert_eq!(s, "PlayStation");
        assert!(m.starts_with("DualSense"));
        assert_eq!(line(&Detected { xbox: 2, ps: vec!["DualShock 4"] }).1, "Xbox + PS");
    }
}
