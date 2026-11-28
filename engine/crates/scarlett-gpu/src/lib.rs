//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod compose;
#[cfg(windows)]
mod gpuprof;
#[cfg(all(windows, feature = "diagnostics"))]
mod skinlog;
#[cfg(windows)]
mod frametl;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
// The process's allocator, which says so when memory runs out
// (`scarlett_core::oom`); in diagnostics builds `copystat`'s, which counts
// blocks too.
#[cfg(all(windows, not(feature = "diagnostics")))]
#[global_allocator]
static HEAP: scarlett_core::oom::Reporting = scarlett_core::oom::Reporting;
#[cfg(all(windows, feature = "diagnostics"))]
mod faultbook;
#[cfg(windows)]
mod cores;
pub mod descriptor;
pub mod fastboot;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
pub mod fence;
pub mod frame;
#[cfg(all(windows, feature = "diagnostics"))]
mod book;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod land;
pub mod list;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod picked;
#[cfg(windows)]
mod pool;
#[cfg(all(windows, feature = "diagnostics"))]
mod popwatch;
#[cfg(windows)]
mod primitive;
pub mod query;
pub mod queue;
#[cfg(all(windows, feature = "diagnostics"))]
pub mod replay;
#[cfg(windows)]
mod residency;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod safecopy;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod tiles;
pub mod signature;
mod switch;
#[cfg(all(windows, feature = "diagnostics"))]
mod ab;
#[cfg(windows)]
mod tdr;
#[cfg(windows)]
mod texture;
pub mod timeline;
#[cfg(windows)]
pub mod titlewrite;
#[cfg(windows)]
mod upload;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod texback;

#[cfg(windows)]
pub use host::on_failure;

/// Whether `switch` is on now, if it is one of those `SCARLETT_AB` turns on and
/// off while the title runs (`ab`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    ab::pick(switch)
}

/// What the frame is at (`copystat::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    copystat::phase()
}
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view};
#[cfg(windows)]
pub use watch::{
    kernel_writing, title_memory_allocated, title_memory_freed, title_memory_reading, title_memory_remapped,
    title_memory_remapping, title_memory_splitting, title_memory_unmapping, title_memory_written,
    writing as kernel_will_write,
};

/// The unit tests opt into the Agility SDK as `scarlett` does: Windows 10's
/// own D3D12 refuses root signatures with the heaps directly indexed (shader
/// model 6.6) as "The parameter is incorrect". The test executable loads
/// `.\D3D12\D3D12Core.dll` beside it (`deps\D3D12` in the target folder,
/// which `build.rs` fills from `scarlett.exe`'s; without one the OS runtime
/// is used as before).
#[cfg(all(test, scarlett_agility_tests))]
mod agility_opt_in {
    /// Linker directives: MSVC's link.exe exports `#[no_mangle]` statics of
    /// an executable only when told to, and a build script's link arguments
    /// do not reach a library's unit-test executable.
    #[cfg(target_env = "msvc")]
    #[used]
    #[unsafe(link_section = ".drectve")]
    static EXPORTS: [u8; 56] = *b" /EXPORT:D3D12SDKVersion,DATA /EXPORT:D3D12SDKPath,DATA ";

    #[repr(transparent)]
    pub struct SdkPath(*const u8);
    // SAFETY: a pointer to a constant string.
    unsafe impl Sync for SdkPath {}

    #[used]
    #[unsafe(no_mangle)]
    #[allow(non_upper_case_globals)]
    pub static D3D12SDKVersion: u32 = 614;

    #[used]
    #[unsafe(no_mangle)]
    #[allow(non_upper_case_globals)]
    pub static D3D12SDKPath: SdkPath = SdkPath(c".\\D3D12\\".as_ptr().cast());
}
