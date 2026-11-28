//! Not a test of the translator: how long a translated compute program takes
//! on the card. `SCARLETT_BENCH=<folder>` and `--ignored bench`: each `.cdxil`
//! there (a console container) is translated as a run would, bound with zeroed
//! user data, root views of `SCARLETT_BENCH_CB` (`dword=value,...` in every
//! root view, default none) and `SCARLETT_BENCH_VIEWS` raw UAVs of
//! `SCARLETT_BENCH_DWORDS` dwords each (16 and 8M; with `SCARLETT_BENCH_BIG=<n>,...` only
//! those heap slots, the rest 64 dwords), and dispatched
//! `SCARLETT_BENCH_GROUPS` (`x,y,z`, default 4096,1,1) times one and times 21:
//! the difference over twenty is the time of one dispatch (the translation's
//! switches apply: compare runs with and without them).

//!
//! Windows 10's own D3D12 has no SM 6.6: the test executable opts into the
//! Agility SDK as `scarlett` does, which takes `.\D3D12\D3D12Core.dll` beside
//! it (`target\release\deps\D3D12`, which `build.rs` fills from
//! `target\release\D3D12`). Without that folder it does not opt in (D3D12
//! would not start at all) and the card's tests run on the OS runtime.

use super::host::{DISPATCH_REPEAT, Dispatch, Host, RAW_UAV_IN_SYSTEM_MEMORY, Root, View};
use crate::native::RootType;

/// Linker directives: MSVC's link.exe exports `#[no_mangle]` statics of an
/// executable only when told to, and a build script's link arguments do not
/// reach a library's unit-test executable.
#[cfg(all(target_env = "msvc", scarlett_agility_tests))]
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

fn numbers(name: &str) -> Vec<u32> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').filter_map(|n| n.trim().parse().ok()).collect())
        .unwrap_or_default()
}

#[test]
#[ignore = "a tool: SCARLETT_BENCH=<folder>"]
fn bench() {
    let Some(folder) = std::env::var_os("SCARLETT_BENCH") else { return };
    let host = Host::new(false).expect("a card");
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .expect("the folder")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "cdxil"))
        .collect();
    files.sort();
    let groups = match numbers("SCARLETT_BENCH_GROUPS")[..] {
        [x, y, z] => [x, y, z],
        [x, y] => [x, y, 1],
        [x] => [x, 1, 1],
        _ => [4096, 1, 1],
    };
    let views = numbers("SCARLETT_BENCH_VIEWS").first().copied().unwrap_or(16) as usize;
    let dwords = numbers("SCARLETT_BENCH_DWORDS").first().copied().unwrap_or(8 << 20);
    let mut cb = vec![0u32; 16384];
    if let Ok(spec) = std::env::var("SCARLETT_BENCH_CB") {
        for pair in spec.split(',') {
            if let Some((at, value)) = pair.split_once('=')
                && let (Ok(at), Ok(value)) = (at.trim().parse::<usize>(), value.trim().parse::<i64>())
                && at < cb.len()
            {
                cb[at] = value as u32;
            }
        }
    }
    // `SCARLETT_BENCH_SYSMEM=1`: the UAVs in system memory, read over the bus.
    RAW_UAV_IN_SYSTEM_MEMORY.set(std::env::var_os("SCARLETT_BENCH_SYSMEM").is_some_and(|v| v == "1"));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for file in files {
        let name = file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let bytes = std::fs::read(&file).expect("the file");
        let shader = match super::translate(&bytes) {
            Ok(s) => s,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        println!(
            "{name}: {} bytes of DXIL; user data {:?}; root CBVs {:?} SRVs {:?} UAVs {:?}; indexed {}",
            shader.dxil.len(),
            shader.user_data.iter().map(|u| (u.index, u.slot, u.dword, matches!(u.root, RootType::Constants))).collect::<Vec<_>>(),
            shader.root_cbvs,
            shader.root_srvs,
            shader.root_uavs,
            shader.root_indexed_views.len(),
        );
        let (root, pso) = match host.compute_pipeline(&shader) {
            Ok(made) => made,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let mut roots = Vec::new();
        let ud = shader.user_data_dwords() as usize;
        if ud > 0 {
            let slot = shader.user_data.first().map_or(0, |u| u32::from(u.slot));
            roots.push((slot, Root::Constants(vec![0; ud])));
        }
        // Root views follow the user data, by slot then kind (`root_signature_for`).
        let mut kinds: Vec<(u8, bool)> =
            shader.root_cbvs.iter().map(|&r| (r, false)).chain(shader.root_srvs.iter().map(|&r| (r, true))).collect();
        kinds.sort_unstable();
        for (n, &(_, srv)) in kinds.iter().enumerate() {
            roots.push((1 + n as u32, Root::View { srv, data: cb.clone() }));
        }
        let big = numbers("SCARLETT_BENCH_BIG");
        let heap: Vec<View> = (0..views)
            .map(|n| View::RawUav(if big.is_empty() || big.contains(&(n as u32)) { dwords } else { 64 }))
            .collect();
        let time = |repeat: u32| {
            DISPATCH_REPEAT.set(repeat);
            let began = std::time::Instant::now();
            let ok = host.dispatch(&root, &pso, &Dispatch { roots: roots.iter().map(|(p, r)| (*p, r.clone())).collect(), views: &heap, samplers: &[], groups });
            DISPATCH_REPEAT.set(1);
            ok.map(|_| began.elapsed().as_secs_f64() * 1e3)
        };
        // `SCARLETT_BENCH_REPEAT`: the longer run's dispatches (21).
        let many = numbers("SCARLETT_BENCH_REPEAT").first().copied().unwrap_or(21).max(2);
        let runs: Vec<_> = (0..3).map(|_| (time(1), time(many))).collect();
        // (the fastest of each length: the runs' own overheads vary)
        let fastest = |pick: fn(&(Result<f64, String>, Result<f64, String>)) -> Option<f64>| {
            runs.iter().filter_map(pick).fold(f64::INFINITY, f64::min)
        };
        let best = (fastest(|r| r.1.as_ref().ok().copied()) - fastest(|r| r.0.as_ref().ok().copied())) / f64::from(many - 1);
        match &runs[0].0 {
            Err(e) => println!("{name}: {e}"),
            Ok(_) => println!("{name}: {best:.3} ms a dispatch of {groups:?} groups"),
        }
    }
}
