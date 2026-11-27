//! NVIDIA DLSS Super Resolution through NVIDIA Streamline, loaded at run
//! time from a folder of the user's; off by default. Upscaling only: no
//! frame generation.
//!
//! - `SCARLETT_DLSS=1`: the title's FidelityFX upscaling (its `ffxDispatch`,
//!   served by `scarlett_hle::modules::fidelityfx`) is done by DLSS instead,
//!   from the same colour, depth, motion vectors, exposure and reactive mask
//!   into the same output, at the same place of the title's command list.
//!   `SCARLETT_DLSS_MODE` (dlaa, quality, balanced, performance,
//!   ultraperformance) is the render scale the title's renderer is patched
//!   to (`scarlett_loader::resolution`); the DLSS mode follows the sizes the
//!   title then passes. `SCARLETT_DLSS_PRESET` (default, e, f, j, k, l, m)
//!   picks the model.
//! - `SCARLETT_STREAMLINE_DIR`: the folder of `sl.interposer.dll`, its
//!   plugins and NGX's DLLs (the launcher fetches them from NVIDIA's GitHub
//!   into the data folder; none of NVIDIA's files are part of this project).
//!
//! Anything missing (the folder, a signature, the feature on this GPU, the
//! device hand-over) leaves the title's own upscaling as it was, with a
//! warning in the log.
//!
//! Streamline is used with manual hooking: nothing of D3D12 or DXGI is
//! hooked; Streamline is only given our device (`slSetD3DDevice`) and
//! records its evaluation into our command lists.
//!
//! Ported from trustno1-coder/VIctory-NVIDIA (`crates/victory-gpu/src/streamline.rs`,
//! GPL-3.0-or-later), its DLSS Super Resolution part only.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub(crate) mod abi;
pub mod plan;

pub use plan::Mode;

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::object::Object;

const INTERPOSER: &str = "sl.interposer.dll";
/// The project id NGX is given (engine "custom", no NVIDIA application id).
const PROJECT_ID: &CStr = c"6f0d7b2e-3a41-4c1e-9b57-2d8e4f1a9c30";
const ENGINE_VERSION: &CStr = c"ScarlettPort 0.5";
/// The viewport of the upscaling.
const VIEWPORT_DLSS: u32 = 1;

/// What the user asked for, read from the environment once.
#[derive(Clone, Debug)]
pub struct Asked {
    pub dlss: bool,
    pub mode: Mode,
    pub preset: u32,
    /// `SCARLETT_STREAMLINE_DIR`; `None` if unset (the caller may name a
    /// default).
    pub dir: Option<PathBuf>,
    /// `SCARLETT_DLSS_JITTER` and `SCARLETT_DLSS_MV`: sign or scale fixes.
    pub jitter: [f32; 2],
    pub mvec: [f32; 2],
    /// `SCARLETT_DLSS_REACTIVE=0`: the reactive mask is not given to DLSS.
    pub reactive: bool,
    /// `SCARLETT_STREAMLINE_LOG=<folder>`: Streamline's own log there, verbose.
    pub log_dir: Option<PathBuf>,
}

impl Asked {
    /// The switches; an unknown mode or preset name is an error.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let on = |name: &str| var(name).is_some_and(|v| v.trim() == "1");
        let mode = match var("SCARLETT_DLSS_MODE") {
            Some(name) => Mode::from_name(&name).ok_or_else(|| {
                format!("SCARLETT_DLSS_MODE={name}: not one of dlaa, quality, balanced, performance, ultraperformance")
            })?,
            None => Mode::Quality,
        };
        let preset = match var("SCARLETT_DLSS_PRESET") {
            Some(name) => plan::preset(&name)
                .ok_or_else(|| format!("SCARLETT_DLSS_PRESET={name}: not one of default, e, f, j, k, l, m"))?,
            None => 0,
        };
        Ok(Asked {
            dlss: on("SCARLETT_DLSS"),
            mode,
            preset,
            dir: var("SCARLETT_STREAMLINE_DIR").map(PathBuf::from),
            jitter: plan::pair(var("SCARLETT_DLSS_JITTER").as_deref()),
            mvec: plan::pair(var("SCARLETT_DLSS_MV").as_deref()),
            reactive: var("SCARLETT_DLSS_REACTIVE").is_none_or(|v| v.trim() != "0"),
            log_dir: var("SCARLETT_STREAMLINE_LOG").filter(|v| v.trim() != "0").map(PathBuf::from),
        })
    }
}

/// What [`init`] made available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Available {
    pub dlss: bool,
}

/// One upscaling the title asked for: a FidelityFX `ffxDispatch` of an
/// upscale, in its own terms (`scarlett_hle::modules::fidelityfx` reads it).
#[derive(Clone, Copy, Debug)]
pub struct Upscale {
    /// The FidelityFX context.
    pub context: usize,
    /// The title's command list.
    pub list: *const Object,
    /// Colour, depth, motion vectors, exposure, reactive mask, transparency
    /// and composition mask, output: the title's resource (null if none) and
    /// the FidelityFX state it is in.
    pub resources: [(*const Object, u32); 7],
    pub camera: plan::Camera,
    pub pre_exposure: f32,
    pub frame_ms: f32,
}

// SAFETY: pointers to title objects that live until the list has run (the
// title's part, as on the console); nothing else is shared.
unsafe impl Send for Upscale {}

pub const COLOUR: usize = 0;
pub const DEPTH: usize = 1;
pub const MOTION: usize = 2;
pub const EXPOSURE: usize = 3;
pub const REACTIVE: usize = 4;
pub const OUTPUT: usize = 6;

/// What became of an upscaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// DLSS does it: the title's own must not run.
    Dlss,
    /// Not taken: the title's own runs. Why, for the log.
    No(&'static str),
}
struct Library(windows_sys::Win32::Foundation::HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this owns the reference LoadLibraryExW took.
        unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0) };
    }
}

// SAFETY: a module handle.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}


/// The library's exported functions.
struct Sl {
    _library: Library,
    init: abi::SlInit,
    is_feature_supported: abi::SlIsFeatureSupported,
    evaluate_feature: abi::SlEvaluateFeature,
    free_resources: abi::SlFreeResources,
    get_feature_requirements: abi::SlGetFeatureRequirements,
    get_feature_version: abi::SlGetFeatureVersion,
    set_constants: abi::SlSetConstants,
    get_feature_function: abi::SlGetFeatureFunction,
    get_new_frame_token: abi::SlGetNewFrameToken,
    set_d3d_device: abi::SlSetD3DDevice,
}

static SL: OnceLock<Sl> = OnceLock::new();

/// What is on once the library is in: the switches and whether DLSS is.
struct Plan {
    asked: Asked,
    dlss: bool,
}

static PLAN: OnceLock<Plan> = OnceLock::new();

/// The DLSS functions, once the device is set.
#[derive(Default)]
struct Functions {
    dlss_optimal: Option<abi::SlDlssGetOptimalSettings>,
    dlss_set_options: Option<abi::SlDlssSetOptions>,
}

static FUNCTIONS: OnceLock<Functions> = OnceLock::new();
static DLSS_READY: AtomicBool = AtomicBool::new(false);

/// Library calls that are not thread safe, and what the upscaling keeps
/// between frames, one caller at a time.
#[derive(Default)]
struct Calls {
    /// Optimal render sizes by output size and mode.
    optimal: Vec<([u32; 2], u32, Option<[u32; 2]>)>,
    /// The mode, sizes and preset the last options named.
    last: Option<(u32, [u32; 2], [u32; 2])>,
}

static CALLS: Mutex<Calls> = Mutex::new(Calls { optimal: Vec::new(), last: None });

fn calls() -> std::sync::MutexGuard<'static, Calls> {
    CALLS.lock().unwrap_or_else(|e| e.into_inner())
}

// Counters since the last frame-rate line: upscalings done by DLSS, failed,
// left to the title; and in all.
static EVALUATED: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static FORWARDED: AtomicU64 = AtomicU64::new(0);
static EVALUATED_ALL: AtomicU64 = AtomicU64::new(0);
/// Upscalings noted so far (their ids).
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Loads Streamline from `asked.dir` when DLSS is asked for; call once,
/// before the host device is made and before the title's imports are bound.
/// Fails soft: what is not there is logged and left off.
pub fn init(asked: &Asked) -> Available {
    if !asked.dlss {
        return Available::default();
    }
    match load_and_init(asked) {
        Ok(available) => available,
        Err(why) => {
            tracing::warn!(
                folder = %asked.dir.as_deref().map(|d| d.display().to_string()).unwrap_or_default(),
                "Streamline: {why}; DLSS stays off, the title's own upscaling runs"
            );
            Available::default()
        }
    }
}

fn load_and_init(asked: &Asked) -> Result<Available, String> {
    let dir = asked.dir.as_deref().ok_or("no folder named (SCARLETT_STREAMLINE_DIR)")?;
    let path = std::path::absolute(dir.join(INTERPOSER)).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!("no {INTERPOSER} in that folder (SCARLETT_STREAMLINE_DIR)"));
    }
    verify_signature(&path)?;
    let sl = load(&path)?;
    let sl = SL.get_or_init(|| sl);
    let dir_wide: Vec<u16> = wide(dir);
    let paths = [dir_wide.as_ptr()];
    let log_wide = asked.log_dir.as_deref().map(wide);
    let features = [abi::FEATURE_DLSS];
    let preferences = abi::Preferences {
        base: abi::BaseStructure::new(abi::PREFERENCES, 1),
        show_console: false,
        log_level: if asked.log_dir.is_some() { abi::LOG_VERBOSE } else { abi::LOG_DEFAULT },
        paths_to_plugins: paths.as_ptr(),
        num_paths_to_plugins: 1,
        path_to_logs_and_data: log_wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
        allocate_callback: std::ptr::null(),
        release_callback: std::ptr::null(),
        log_message_callback: Some(log_message),
        // No downloads (OTA): only what is in the folder runs.
        flags: abi::DISABLE_CL_STATE_TRACKING
            | abi::DISABLE_DEBUG_TEXT
            | abi::USE_MANUAL_HOOKING
            | abi::USE_DXGI_FACTORY_PROXY
            | abi::USE_FRAME_BASED_RESOURCE_TAGGING,
        features_to_load: features.as_ptr(),
        num_features_to_load: features.len() as u32,
        application_id: 0,
        engine: abi::ENGINE_CUSTOM,
        engine_version: ENGINE_VERSION.as_ptr(),
        project_id: PROJECT_ID.as_ptr(),
        render_api: abi::RENDER_API_D3D12,
    };
    // SAFETY: a filled preferences structure; the library copies what it keeps.
    let code = unsafe { (sl.init)(&preferences, abi::SDK_VERSION) };
    if code != 0 {
        return Err(format!("slInit failed: {} ({code})", abi::result_name(code)));
    }
    let adapter = default_adapter();
    let supported = match adapter.as_ref().map(|a| a.1) {
        None => Err("no adapter".to_owned()),
        Some(luid) => {
            let info = abi::AdapterInfo {
                base: abi::BaseStructure::new(abi::ADAPTER_INFO, 1),
                device_luid: std::ptr::from_ref(&luid).cast(),
                device_luid_size: size_of_val(&luid) as u32,
                vk_physical_device: std::ptr::null_mut(),
            };
            // SAFETY: an adapter description that lives through the call.
            let code = unsafe { (sl.is_feature_supported)(abi::FEATURE_DLSS, &info) };
            if code == 0 { Ok(()) } else { Err(format!("{} ({code})", abi::result_name(code))) }
        }
    };
    let dlss = report("DLSS Super Resolution", abi::FEATURE_DLSS, supported, sl);
    tracing::info!(
        library = %path.display(),
        sdk = "2.14.16",
        adapter = adapter.as_ref().map_or("?", |a| a.0.as_str()),
        dlss,
        mode = asked.mode.name(),
        preset = asked.preset,
        "Streamline loaded (manual hooking, no downloads)"
    );
    let _ = PLAN.set(Plan { asked: asked.clone(), dlss });
    Ok(Available { dlss })
}

fn report(name: &str, feature: u32, supported: Result<(), String>, sl: &Sl) -> bool {
    let mut requirements: abi::FeatureRequirements = unsafe_zeroed_requirements();
    // SAFETY: a structure to fill.
    let asked = unsafe { (sl.get_feature_requirements)(feature, &mut requirements) };
    let needs = (asked == 0).then(|| {
        format!(
            "OS {} (needs {}), driver {} (needs {}){}{}",
            requirements.os_version_detected,
            requirements.os_version_required,
            requirements.driver_version_detected,
            requirements.driver_version_required,
            if requirements.flags & abi::REQUIRES_HARDWARE_SCHEDULING != 0 { ", hardware GPU scheduling" } else { "" },
            if requirements.flags & abi::REQUIRES_VSYNC_OFF != 0 { ", v-sync off" } else { "" },
        )
    });
    match supported {
        Ok(()) => {
            tracing::info!(requirements = needs.as_deref().unwrap_or("?"), "Streamline: {name} is supported on this GPU");
            true
        }
        Err(why) => {
            tracing::warn!(requirements = needs.as_deref().unwrap_or("?"), "Streamline: {name} is not supported here: {why}");
            false
        }
    }
}

fn unsafe_zeroed_requirements() -> abi::FeatureRequirements {
    // SAFETY: plain data; pointers null, counts zero.
    let mut r: abi::FeatureRequirements = unsafe { std::mem::zeroed() };
    r.base = abi::BaseStructure::new(abi::FEATURE_REQUIREMENTS, 2);
    r
}
unsafe extern "C" fn log_message(kind: u32, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: a NUL-terminated message for the call.
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    let text = text.trim_end();
    // (Every plugin in the folder that was not asked for says so.)
    let noise = text.contains("since it is was not requested by the host");
    match kind {
        abi::LOG_ERROR => tracing::error!(target: "streamline", "{text}"),
        abi::LOG_WARN if !noise => tracing::warn!(target: "streamline", "{text}"),
        _ => tracing::debug!(target: "streamline", "{text}"),
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}
/// The adapter a device made with no adapter named is on (the first), its
/// name and LUID.
fn default_adapter() -> Option<(String, windows::Win32::Foundation::LUID)> {
    // SAFETY: plain DXGI queries.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        Some((String::from_utf16_lossy(&desc.Description[..len]), desc.AdapterLuid))
    }
}
/// The Authenticode signature of `path` (NVIDIA signs its DLLs), checked
/// without going online.
fn verify_signature(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Security::WinTrust::*;
    let file = wide(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: file.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: filled structures that live through both calls.
    let code = unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, (&raw mut data).cast()) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: as above; closes the state the first call opened.
    unsafe { WinVerifyTrust(std::ptr::null_mut(), &mut action, (&raw mut data).cast()) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{} is not validly signed ({:#x})", path.display(), code as u32))
    }
}


fn load(path: &Path) -> Result<Sl, String> {
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    let file = wide(path);
    // Its own folder and System32 only, never the current folder.
    // SAFETY: a NUL-terminated full path.
    let module =
        unsafe { LoadLibraryExW(file.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(format!("{INTERPOSER} did not load ({})", std::io::Error::last_os_error()));
    }
    let library = Library(module);
    let find = |name: &CStr| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("{INTERPOSER} has no {}", name.to_string_lossy()))
    };
    macro_rules! get {
        ($name:literal, $t:ty) => {
            // SAFETY: the export of that name has the header's signature.
            unsafe { std::mem::transmute::<*const c_void, $t>(find($name)?) }
        };
    }
    Ok(Sl {
        init: get!(c"slInit", abi::SlInit),
        is_feature_supported: get!(c"slIsFeatureSupported", abi::SlIsFeatureSupported),
        evaluate_feature: get!(c"slEvaluateFeature", abi::SlEvaluateFeature),
        free_resources: get!(c"slFreeResources", abi::SlFreeResources),
        get_feature_requirements: get!(c"slGetFeatureRequirements", abi::SlGetFeatureRequirements),
        get_feature_version: get!(c"slGetFeatureVersion", abi::SlGetFeatureVersion),
        set_constants: get!(c"slSetConstants", abi::SlSetConstants),
        get_feature_function: get!(c"slGetFeatureFunction", abi::SlGetFeatureFunction),
        get_new_frame_token: get!(c"slGetNewFrameToken", abi::SlGetNewFrameToken),
        set_d3d_device: get!(c"slSetD3DDevice", abi::SlSetD3DDevice),
        _library: library,
    })
}

/// Whether the title's FidelityFX calls are to be served (DLSS is on and
/// Streamline loaded): `scarlett_hle` then puts its own in front of them.
pub fn serving() -> bool {
    PLAN.get().is_some_and(|p| p.dlss)
}

/// Whether DLSS is to take the title's upscale dispatches.
pub fn upscaling() -> bool {
    serving()
}

/// The host device is made: Streamline is given it (`slSetD3DDevice`) and
/// the DLSS functions are fetched.
pub(crate) fn device_made(device: &ID3D12Device) {
    let (Some(plan), Some(sl)) = (PLAN.get(), SL.get()) else { return };
    if !plan.dlss {
        return;
    }
    // SAFETY: the native device.
    let code = unsafe { (sl.set_d3d_device)(device.as_raw()) };
    if code != 0 {
        tracing::warn!("Streamline: slSetD3DDevice failed: {} ({code}); DLSS stays off", abi::result_name(code));
        return;
    }
    let function = |name: &CStr| -> Option<*mut c_void> {
        let mut f = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name and an out pointer.
        let code = unsafe { (sl.get_feature_function)(abi::FEATURE_DLSS, name.as_ptr(), &mut f) };
        if code != 0 || f.is_null() {
            tracing::warn!("Streamline: no {} ({}, {code})", name.to_string_lossy(), abi::result_name(code));
            return None;
        }
        Some(f)
    };
    let functions = Functions {
        // SAFETY: the function of that name has the header's signature.
        dlss_optimal: function(c"slDLSSGetOptimalSettings")
            .map(|f| unsafe { std::mem::transmute::<*mut c_void, abi::SlDlssGetOptimalSettings>(f) }),
        // SAFETY: as above.
        dlss_set_options: function(c"slDLSSSetOptions")
            .map(|f| unsafe { std::mem::transmute::<*mut c_void, abi::SlDlssSetOptions>(f) }),
    };
    let ready = functions.dlss_optimal.is_some() && functions.dlss_set_options.is_some();
    log_version(sl, abi::FEATURE_DLSS, "DLSS Super Resolution");
    let _ = FUNCTIONS.set(functions);
    DLSS_READY.store(ready, Release);
}

fn log_version(sl: &Sl, feature: u32, name: &str) {
    let mut version = abi::FeatureVersion {
        base: abi::BaseStructure::new(abi::FEATURE_VERSION, 1),
        version_sl: abi::Version::default(),
        version_ngx: abi::Version::default(),
    };
    // SAFETY: a structure to fill.
    let code = unsafe { (sl.get_feature_version)(feature, &mut version) };
    if code == 0 {
        tracing::info!(streamline = %version.version_sl, ngx = %version.version_ngx, "Streamline: {name} loaded");
    } else {
        tracing::warn!("Streamline: {name} has no version: {} ({code})", abi::result_name(code));
    }
}
fn frame_token(sl: &Sl, index: u32) -> Option<*mut abi::FrameToken> {
    let mut token = std::ptr::null_mut();
    // SAFETY: an out pointer and an index.
    let code = unsafe { (sl.get_new_frame_token)(&mut token, &index) };
    (code == 0 && !token.is_null()).then_some(token)
}

/// The upscaling `up` asked for on the title's thread: DLSS records it at
/// this place of the title's list instead; the title's own runs otherwise.
pub fn upscale(up: &Upscale) -> Taken {
    let plan = PLAN.get();
    if !(plan.is_some_and(|p| p.dlss) && DLSS_READY.load(Acquire)) {
        return Taken::No(if plan.is_none() {
            "Streamline is not loaded"
        } else {
            "Streamline has no device yet, or DLSS did not start"
        });
    }
    if [COLOUR, DEPTH, MOTION, OUTPUT].iter().any(|&i| up.resources[i].0.is_null()) {
        return Taken::No("a required texture (colour, depth, motion vectors, output) is missing");
    }
    if up.camera.render.contains(&0) || up.camera.output.contains(&0) {
        return Taken::No("an empty render or output size");
    }
    if let Err(why) = validate(up) {
        return Taken::No(why);
    }
    let id = NEXT_ID.fetch_add(1, Relaxed) + 1;
    let up = *up;
    let asked = crate::list::external(up.list, Box::new(move |host| evaluate(host, &up, id)));
    if !asked {
        return Taken::No("the title's list records nowhere (no host list)");
    }
    Taken::Dlss
}
/// The title's own upscaling ran for this reason.
pub fn forwarded() {
    FORWARDED.fetch_add(1, Relaxed);
}

/// What an upscaling's textures must be for DLSS, checked on the title's
/// thread (the title's own runs if not): host textures, in states they can
/// be in.
fn validate(up: &Upscale) -> Result<(), &'static str> {
    for (i, &(resource, state)) in up.resources.iter().enumerate() {
        if resource.is_null() || i == 5 {
            continue;
        }
        if !crate::resource::is_resource(resource) {
            return Err("a texture of the upscaling is no resource of ours");
        }
        let Some(copy) = crate::resource::host_copy(resource) else {
            return Err("a texture of the upscaling has no host copy (a buffer?)");
        };
        let wanted = if i == OUTPUT { plan::output_state(state) } else { plan::input_state(state) };
        if crate::host::legal_state(&copy.texture, wanted) != wanted {
            return Err(if i == OUTPUT {
                "the output cannot be written by a shader (no unordered access)"
            } else {
                "an input cannot be read by a shader"
            });
        }
    }
    Ok(())
}
/// DLSS in place of the title's upscaling, recorded into `host` (where the
/// title's list is now). Whether it recorded anything.
fn evaluate(host: &crate::host::HostList, up: &Upscale, id: u64) -> bool {
    let Some(plan) = PLAN.get() else { return false };
    let camera = up.camera;
    let fail = |why: &str| {
        FAILED.fetch_add(1, Relaxed);
        tracing::warn!(id, "DLSS: the upscaling was not recorded: {why} (the output keeps what it had)");
        false
    };
    // The inputs as a compute shader's would be (aliased views, uploads of
    // title-written textures, depth twins), then in the states DLSS is told.
    let used = |i: usize| {
        !up.resources[i].0.is_null()
            && (i != REACTIVE || plan.asked.reactive)
            && (i != EXPOSURE || camera.flags & plan::FFX_AUTO_EXPOSURE == 0)
    };
    let mut textures: Textures = Default::default();
    for i in [COLOUR, DEPTH, MOTION, EXPOSURE, REACTIVE, OUTPUT] {
        if !used(i) {
            continue;
        }
        let (resource, state) = up.resources[i];
        if i != OUTPUT && crate::external::prepare_input(host, resource).is_none() {
            return fail("an input could not be prepared");
        }
        let wanted = if i == OUTPUT { plan::output_state(state) } else { plan::input_state(state) };
        let Some((texture, legal)) = crate::external::host_texture(host, resource, wanted) else {
            return fail("a texture has no host copy");
        };
        if legal != wanted {
            return fail("a texture cannot be in the state DLSS needs");
        }
        textures[i] = Some((texture, legal));
    }
    let recorded = match record(host.list.as_raw(), &textures, &camera, up.pre_exposure, id) {
        Ok(recorded) => recorded,
        Err(why) => return fail(&why),
    };
    crate::external::output_written(up.resources[OUTPUT].0);
    EVALUATED.fetch_add(1, Relaxed);
    let all = EVALUATED_ALL.fetch_add(1, Relaxed) + 1;
    if all == 1 || recorded.changed {
        tracing::info!(
            render = ?camera.render,
            output = ?camera.output,
            mode = plan::sl_mode_name(recorded.mode),
            asked = plan.asked.mode.name(),
            preset = plan.asked.preset,
            optimal = ?recorded.optimal,
            hdr = camera.flags & plan::FFX_HIGH_DYNAMIC_RANGE != 0,
            depth_inverted = camera.depth_inverted(),
            auto_exposure = textures[EXPOSURE].is_none(),
            reactive = textures[REACTIVE].is_some(),
            display_motion_vectors = camera.display_motion_vectors(),
            jitter = ?recorded.jitter,
            mvec_scale = ?recorded.mvec_scale,
            near = camera.near,
            far = camera.far,
            fov = camera.fov_y,
            formats = ?formats(&textures),
            evaluations = all,
            "DLSS: upscaling recorded in place of the title's FidelityFX"
        );
    }
    true
}

/// Colour, depth, motion vectors, exposure, reactive mask, (transparency),
/// output: the host textures DLSS is given and the states they are in.
type Textures = [Option<(ID3D12Resource, u32)>; 7];

fn formats(textures: &Textures) -> Vec<i32> {
    textures
        .iter()
        .flatten()
        .map(|(t, _)| {
            // SAFETY: a live texture.
            unsafe { t.GetDesc() }.Format.0
        })
        .collect()
}

/// What [`record`] recorded.
struct Recorded {
    mode: u32,
    optimal: Option<[u32; 2]>,
    /// Another mode or size than the last time.
    changed: bool,
    jitter: [f32; 2],
    mvec_scale: [f32; 2],
}

/// The DLSS evaluation of `textures` recorded into the open command list
/// `list`: the mode whose render size holds the title's, the options, the
/// frame's constants (frame `id` of the upscaling's viewport), the textures
/// as local tags. The textures are left in the states given.
fn record(list: *mut c_void, textures: &Textures, camera: &plan::Camera, pre_exposure: f32, id: u64) -> Result<Recorded, String> {
    let (Some(plan), Some(sl), Some(functions)) = (PLAN.get(), SL.get(), FUNCTIONS.get()) else {
        return Err("Streamline is not ready".into());
    };
    let (Some(optimal_of), Some(set_options)) = (functions.dlss_optimal, functions.dlss_set_options) else {
        return Err("no DLSS functions".into());
    };
    let mut calls = calls();
    let output = camera.output;
    let optimal = |calls: &mut Calls, mode: u32| -> Option<[u32; 2]> {
        if let Some(&(_, _, size)) = calls.optimal.iter().find(|(o, m, _)| *o == output && *m == mode) {
            return size;
        }
        let options = abi::DlssOptions::new(mode, output, plan.asked.preset);
        let mut settings = abi::DlssOptimalSettings::default();
        // SAFETY: filled options and a structure to fill.
        let code = unsafe { optimal_of(&options, &mut settings) };
        let size = (code == 0 && settings.optimal_render_width > 0)
            .then_some([settings.optimal_render_width, settings.optimal_render_height]);
        calls.optimal.push((output, mode, size));
        size
    };
    let sizes: Vec<(u32, Option<[u32; 2]>)> = plan::MODES.iter().map(|&m| (m, optimal(&mut calls, m))).collect();
    let optimal_of_mode = |m: u32| sizes.iter().find(|s| s.0 == m).and_then(|s| s.1);
    let mode = plan::pick_mode(camera.render, output, optimal_of_mode)
        .ok_or_else(|| format!("no DLSS mode takes a {:?} render for a {output:?} output", camera.render))?;
    let mut options = abi::DlssOptions::new(mode, output, plan.asked.preset);
    options.pre_exposure = if pre_exposure > 0.0 { pre_exposure } else { 1.0 };
    options.color_buffers_hdr = abi::boolean(camera.flags & plan::FFX_HIGH_DYNAMIC_RANGE != 0);
    options.use_auto_exposure = abi::boolean(textures[EXPOSURE].is_none());
    let viewport = abi::ViewportHandle::new(VIEWPORT_DLSS);
    // SAFETY: filled options for our viewport.
    let code = unsafe { set_options(&viewport, &options) };
    if code != 0 {
        return Err(format!("slDLSSSetOptions: {} ({code})", abi::result_name(code)));
    }
    let changed = calls.last != Some((mode, camera.render, output));
    calls.last = Some((mode, camera.render, output));
    let token = frame_token(sl, id as u32).ok_or("no frame token")?;
    let constants = camera.constants(plan.asked.jitter, plan.asked.mvec);
    // SAFETY: constants for this frame and viewport.
    let code = unsafe { (sl.set_constants)(&constants, token, &viewport) };
    if code != 0 {
        return Err(format!("slSetConstants: {} ({code})", abi::result_name(code)));
    }
    let kinds = [
        (COLOUR, abi::BUFFER_SCALING_INPUT_COLOR, camera.render),
        (DEPTH, abi::BUFFER_DEPTH, camera.render),
        (MOTION, abi::BUFFER_MOTION_VECTORS, camera.motion_size()),
        (EXPOSURE, abi::BUFFER_EXPOSURE, [1, 1]),
        (REACTIVE, abi::BUFFER_BIAS_CURRENT_COLOR_HINT, camera.render),
        (OUTPUT, abi::BUFFER_SCALING_OUTPUT_COLOR, output),
    ];
    let mut resources: Vec<(abi::Resource, u32, [u32; 2])> = kinds
        .iter()
        .filter_map(|&(i, kind, size)| {
            textures[i].as_ref().map(|(t, state)| (abi::Resource::texture(t.as_raw(), *state), kind, size))
        })
        .collect();
    let mut tags: Vec<abi::ResourceTag> = resources
        .iter_mut()
        .map(|(r, kind, size)| abi::ResourceTag::new(r, *kind, abi::VALID_UNTIL_EVALUATE, abi::Extent::sized(*size)))
        .collect();
    let mut inputs: Vec<*const abi::BaseStructure> = vec![std::ptr::from_ref(&viewport.base)];
    inputs.extend(tags.iter_mut().map(|t| std::ptr::from_ref(&t.base)));
    // SAFETY: an open command list; inputs that live through the call.
    let code = unsafe { (sl.evaluate_feature)(abi::FEATURE_DLSS, token, inputs.as_ptr(), inputs.len() as u32, list) };
    if code != 0 {
        return Err(format!("slEvaluateFeature: {} ({code})", abi::result_name(code)));
    }
    Ok(Recorded { mode, optimal: optimal_of_mode(mode), changed, jitter: constants.jitter_offset, mvec_scale: constants.mvec_scale })
}

/// The title's FidelityFX upscaler `context` is destroyed: what DLSS made
/// for it goes once submitted work has run.
pub fn upscaler_destroyed(context: usize) {
    let (Some(sl), true) = (SL.get(), DLSS_READY.load(Acquire)) else { return };
    let _ = context;
    crate::external::finish_submitted();
    let viewport = abi::ViewportHandle::new(VIEWPORT_DLSS);
    let mut calls = calls();
    // SAFETY: our viewport; nothing that names it is in flight.
    let code = unsafe { (sl.free_resources)(abi::FEATURE_DLSS, &viewport) };
    calls.last = None;
    tracing::info!(code, result = abi::result_name(code), "DLSS: the title's upscaler was destroyed; DLSS let go of its resources");
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    let Some(plan) = PLAN.get() else { return };
    let [done, failed, forwarded] = [&EVALUATED, &FAILED, &FORWARDED].map(|c| c.swap(0, Relaxed));
    if plan.dlss || forwarded > 0 {
        tracing::info!(
            "DLSS per frame: {:.2} upscalings by DLSS, {:.2} failed, {:.2} left to the title's FidelityFX ({} by DLSS so far)",
            done as f64 / frames,
            failed as f64 / frames,
            forwarded as f64 / frames,
            EVALUATED_ALL.load(Relaxed)
        );
    }
}
