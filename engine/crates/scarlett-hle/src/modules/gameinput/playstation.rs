//! PlayStation pads read directly as HID devices: DualSense, DualSense Edge
//! and DualShock 4, over USB or Bluetooth (`SCARLETT_PAD_PS=0`: not read).
//!
//! Windows does not show these pads to XInput, so without this they needed
//! Steam Input or another mapper. A thread of ours finds the first one,
//! reads its input reports and keeps the newest as a console pad; the host
//! source hands it out as pad 0 while no XInput pad is in that slot.
//!
//! Buttons by position, as the console's: Cross A, Circle B, Square X,
//! Triangle Y, Options Menu, Create/Share View, L1/R1 shoulders, L3/R3 stick
//! clicks. Not done: rumble, the light bar, the touch pad and motion.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::core::GUID;

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER, RIGHT_STICK, UP,
    VIEW, X, Y,
};

/// Sony's USB vendor id.
const SONY: u16 = 0x054C;

/// The pads read, by product id, and whether each is a DualSense.
const PADS: [(u16, bool); 4] = [
    (0x0CE6, true),  // DualSense
    (0x0DF2, true),  // DualSense Edge
    (0x05C4, false), // DualShock 4
    (0x09CC, false), // DualShock 4, second model
];

/// A reading older than this is a pad that went away.
const STALE: Duration = Duration::from_millis(500);

/// How often the HID devices are looked through while no pad is open.
const RESCAN: Duration = Duration::from_secs(2);

/// The newest reading and when it came.
static LATEST: Mutex<Option<(PadState, Instant)>> = Mutex::new(None);

/// Whether PlayStation pads are read (`SCARLETT_PAD_PS=0`: not).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCARLETT_PAD_PS").is_none_or(|v| v != "0"))
}

/// The PlayStation pad's state, if one is connected and reporting; starts
/// the reading thread on first use.
pub(super) fn pad() -> Option<PadState> {
    if !on() {
        return None;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("scarlett-ps-pad".into()).spawn(read_forever);
    });
    let latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    latest.filter(|(_, at)| at.elapsed() < STALE).map(|(pad, _)| pad)
}

/// Finds a pad, reads it until it goes, and looks again.
fn read_forever() {
    loop {
        match open_first() {
            Some((handle, dualsense)) => {
                tracing::info!(dualsense, "PlayStation pad connected (SCARLETT_PAD_PS)");
                read_until_gone(handle, dualsense);
                // SAFETY: our handle, closed once.
                unsafe { CloseHandle(handle as HANDLE) };
                *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = None;
                tracing::info!("PlayStation pad disconnected");
            }
            None => std::thread::sleep(RESCAN),
        }
    }
}

/// Reads input reports from an open pad (`handle`, as an integer so the
/// thread may hold it) until a read fails.
fn read_until_gone(handle: isize, dualsense: bool) {
    let mut buf = [0u8; 128];
    loop {
        let mut read = 0u32;
        // SAFETY: a handle opened for synchronous reads, and room for the
        // largest report.
        let ok = unsafe {
            ReadFile(handle as HANDLE, buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut())
        };
        if ok == 0 {
            return;
        }
        let report = &buf[..read as usize];
        let pad = if dualsense { parse_dualsense(report) } else { parse_dualshock4(report) };
        if let Some(pad) = pad {
            *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some((pad, Instant::now()));
        }
    }
}

/// Opens the first Sony pad among the HID devices: its handle (as an
/// integer) and whether it is a DualSense.
fn open_first() -> Option<(isize, bool)> {
    let mut hid = GUID::from_u128(0);
    // SAFETY: an out pointer to a GUID.
    unsafe { HidD_GetHidGuid(&mut hid) };
    // SAFETY: a class GUID; no enumerator or window.
    let set = unsafe {
        SetupDiGetClassDevsW(&hid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
    };
    if set == -1 || set == 0 {
        return None;
    }
    let mut found = None;
    for n in 0.. {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: our device set; a sized structure.
        if unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &hid, n, &mut iface) } == 0 {
            break;
        }
        let Some(path) = interface_path(set, &iface) else {
            continue;
        };
        if let Some(open) = open_if_ours(&path) {
            found = Some(open);
            break;
        }
    }
    // SAFETY: our device set, destroyed once.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    found
}

/// The device path of an interface, NUL-terminated.
fn interface_path(set: isize, iface: &SP_DEVICE_INTERFACE_DATA) -> Option<Vec<u16>> {
    let mut needed = 0u32;
    // SAFETY: asking for the size only.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(set, iface, std::ptr::null_mut(), 0, &mut needed, std::ptr::null_mut())
    };
    if needed < 8 {
        return None;
    }
    // u64s, for the structure's alignment.
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let detail = storage.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: the buffer is `needed` bytes and aligned; cbSize is the
    // structure's fixed part, as the API asks.
    unsafe {
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        if SetupDiGetDeviceInterfaceDetailW(set, iface, detail, needed, std::ptr::null_mut(), std::ptr::null_mut())
            == 0
        {
            return None;
        }
        let path = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
        let max = (needed as usize - 4) / 2;
        let len = (0..max).take_while(|&i| *path.add(i) != 0).count();
        let mut out = std::slice::from_raw_parts(path, len).to_vec();
        out.push(0);
        Some(out)
    }
}

/// Opens `path` if it is one of [`PADS`]; switches a pad on Bluetooth into
/// its full reports.
fn open_if_ours(path: &[u16]) -> Option<(isize, bool)> {
    let open = |access: u32| {
        // SAFETY: a NUL-terminated path; synchronous I/O.
        unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        }
    };
    let mut handle = open(GENERIC_READ | GENERIC_WRITE);
    if handle == INVALID_HANDLE_VALUE {
        handle = open(GENERIC_READ);
    }
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut attributes = HIDD_ATTRIBUTES { Size: size_of::<HIDD_ATTRIBUTES>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: an open HID handle and a sized structure.
    let known = unsafe { HidD_GetAttributes(handle, &mut attributes) };
    let pad = PADS.iter().find(|(product, _)| known && attributes.VendorID == SONY && attributes.ProductID == *product);
    let Some(&(_, dualsense)) = pad else {
        // SAFETY: our handle.
        unsafe { CloseHandle(handle) };
        return None;
    };
    // On Bluetooth both pads send short reports until the calibration
    // feature report is asked for (DualSense 0x05, DualShock 4 0x02); on
    // USB the request is harmless.
    let mut feature = [0u8; 64];
    feature[0] = if dualsense { 0x05 } else { 0x02 };
    // SAFETY: an open handle and a buffer of the length given.
    unsafe { HidD_GetFeature(handle, feature.as_mut_ptr().cast(), feature.len() as u32) };
    Some((handle as isize, dualsense))
}

/// A stick byte (0..=255, 128 centred) as -1..=1.
fn axis(v: u8) -> f32 {
    ((f32::from(v) - 128.0) / 127.0).clamp(-1.0, 1.0)
}

/// The hat switch's low nibble (0 up, then clockwise; 8 none) as buttons.
fn dpad(hat: u8) -> u32 {
    match hat & 0x0F {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => DOWN | RIGHT,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => UP | LEFT,
        _ => 0,
    }
}

/// The console pad from the common fields, wherever a report keeps them:
/// sticks (lx, ly, rx, ry), triggers, the face-and-hat byte, the shoulder
/// byte.
fn pad_from(sticks: [u8; 4], triggers: [u8; 2], face: u8, shoulders: u8) -> PadState {
    let mut buttons = dpad(face);
    for (bit, button) in [(0x10, X), (0x20, A), (0x40, B), (0x80, Y)] {
        if face & bit != 0 {
            buttons |= button;
        }
    }
    for (bit, button) in [
        (0x01, LEFT_SHOULDER),
        (0x02, RIGHT_SHOULDER),
        (0x10, VIEW),
        (0x20, MENU),
        (0x40, LEFT_STICK),
        (0x80, RIGHT_STICK),
    ] {
        if shoulders & bit != 0 {
            buttons |= button;
        }
    }
    PadState {
        buttons,
        left_trigger: f32::from(triggers[0]) / 255.0,
        right_trigger: f32::from(triggers[1]) / 255.0,
        left_x: axis(sticks[0]),
        // Up is 0 on these pads, positive on the console's.
        left_y: -axis(sticks[1]),
        right_x: axis(sticks[2]),
        right_y: -axis(sticks[3]),
    }
}

/// A DualSense input report: USB (id 0x01, 64 bytes), Bluetooth full (id
/// 0x31) or Bluetooth short (id 0x01, about 10 bytes).
fn parse_dualsense(r: &[u8]) -> Option<PadState> {
    match (r.first()?, r.len()) {
        // Sticks at 1..5, triggers 5 and 6, buttons from 8.
        (0x01, n) if n >= 11 => Some(pad_from([r[1], r[2], r[3], r[4]], [r[5], r[6]], r[8], r[9])),
        // The same one byte on, after a sequence byte.
        (0x31, n) if n >= 12 => Some(pad_from([r[2], r[3], r[4], r[5]], [r[6], r[7]], r[9], r[10])),
        // Short Bluetooth: sticks 1..5, buttons 5 and 6, triggers 8 and 9.
        (0x01, n) if n >= 10 => Some(pad_from([r[1], r[2], r[3], r[4]], [r[8], r[9]], r[5], r[6])),
        _ => None,
    }
}

/// A DualShock 4 input report: USB or short Bluetooth (id 0x01), or
/// Bluetooth full (id 0x11, two bytes on).
fn parse_dualshock4(r: &[u8]) -> Option<PadState> {
    match (r.first()?, r.len()) {
        (0x01, n) if n >= 10 => Some(pad_from([r[1], r[2], r[3], r[4]], [r[8], r[9]], r[5], r[6])),
        (0x11, n) if n >= 12 => Some(pad_from([r[3], r[4], r[5], r[6]], [r[10], r[11]], r[7], r[8])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dualsense_usb_report_maps_to_console_buttons() {
        let mut r = [0u8; 64];
        r[0] = 0x01;
        r[1..5].copy_from_slice(&[128, 0, 255, 128]);
        r[5] = 255; // L2
        r[8] = 0x20 | 2; // Cross, hat right
        r[9] = 0x20 | 0x01; // Options, L1
        let pad = parse_dualsense(&r).unwrap();
        assert_eq!(pad.buttons, A | RIGHT | MENU | LEFT_SHOULDER);
        assert!(pad.left_y > 0.99, "stick pushed up reads positive");
        assert!(pad.right_x > 0.99);
        assert!((pad.left_trigger - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dualsense_bluetooth_full_report_is_one_byte_on() {
        let mut r = [0u8; 78];
        r[0] = 0x31;
        r[2..6].copy_from_slice(&[128, 128, 128, 128]);
        r[9] = 0x80 | 8; // Triangle, hat none
        r[10] = 0x10; // Create
        let pad = parse_dualsense(&r).unwrap();
        assert_eq!(pad.buttons, Y | VIEW);
        assert!(pad.left_x.abs() < 0.01);
    }

    #[test]
    fn dualshock4_reports_map_alike() {
        let mut usb = [0u8; 64];
        usb[0] = 0x01;
        usb[1..5].copy_from_slice(&[128, 128, 128, 128]);
        usb[5] = 0x40 | 8; // Circle
        usb[6] = 0x80; // R3
        assert_eq!(parse_dualshock4(&usb).unwrap().buttons, B | RIGHT_STICK);
        let mut bt = [0u8; 78];
        bt[0] = 0x11;
        bt[3..7].copy_from_slice(&[128, 128, 128, 128]);
        bt[7] = 0x10 | 4; // Square, hat down
        assert_eq!(parse_dualshock4(&bt).unwrap().buttons, X | DOWN);
    }
}
