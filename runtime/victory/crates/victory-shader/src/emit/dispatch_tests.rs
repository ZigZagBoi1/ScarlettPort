//! Checks GPU stores and wave operations at the edge of a thread-count dispatch.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower};
use crate::native::{HwStage, NativeProgram, RootType, StageIo};
use crate::{io, resource, structure};

/// The DXBC containers in a file: each found by its magic and the size its header gives.
fn containers(bytes: &[u8]) -> Vec<&[u8]> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(i) = bytes[at..].windows(4).position(|w| w == b"DXBC") {
        let start = at + i;
        let size = bytes
            .get(start + 24..start + 28)
            .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        if size >= 64 && start + size <= bytes.len() {
            found.push(&bytes[start..start + size]);
            at = start + size;
        } else {
            at = start + 4;
        }
    }
    found
}

/// A compute program of `threads` threads a group (a power of two; `wave64`: in console waves
/// of 64 lanes) that stores, at dword `group * threads + thread` of its root UAV (root
/// parameter 0, user data 0 and 1, whose descriptor it builds as the builder's programs do),
/// the thread's index in its group plus 1. The group id arrives in s2, after the address.
fn numbering(wave64: bool, threads: u32) -> NativeProgram {
    use crate::native::{ComputeIo, ResourceType, RootParam, UsedResource};
    let words: Vec<u32> = vec![
        0x8F0C_0002 | (0x80 + threads.trailing_zeros()) << 8, // s_lshl_b32 s12, s2, log2(threads)
        0x8801_FF01,
        0x0001_0000, // s_or_b32 s1, s1, 0x10000 (a stride of 1)
        0xBE82_03C1, // s_mov_b32 s2, -1 (records)
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, 0x11014fac
        0x4A02_000C, // v_add_nc_u32 v1, s12, v0
        0x3404_0282, // v_lshlrev_b32 v2, 2, v1
        0x4A06_0081, // v_add_nc_u32 v3, 1, v0
        0x3408_0082, // v_lshlrev_b32 v4, 2, v0
        0xD834_0000, 0x0000_0304, // ds_write_b32 v4, v3
        0xBF8A_0000, // s_barrier
        0x7E0A_0280, // v_mov_b32 v5, 0
        0xD8D8_0000, 0x0300_0005, // ds_read_b32 v3, v5 (lane zero wrote 1)
        0x4A06_0700, // v_add_nc_u32 v3, v0, v3
        0xE070_1000,
        0x8000_0302, // buffer_store_dword v3, v2, s[0:3], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            sgprs: 2,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 1,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        // two user SGPRs (the address), then the group id x
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1 | 1 << 7 | 1 << 15, // 512 bytes of LDS
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered for a host of 32-lane waves; `limited`: for dispatches by thread counts
/// ([`lower::with_thread_limits`]).
fn lowered_for(native: &NativeProgram, limited: bool) -> Result<HostShader, crate::ShaderError> {
    let p = crate::spill::build(native)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, native)?;
    let parts = lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        dropped: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    };
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = if limited {
        lower::with_thread_limits(|| lower::lower(&parts))
    } else {
        lower::lower(&parts)
    };
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        half_views: lowered.half_views,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    })
}

/// The console's `DispatchX` with `USE_THREAD_DIMENSIONS` counts threads: the last group is
/// cut short, and no thread past the count runs (the builder rebuilds the levels of its
/// pre-built trees in place, one node per thread, and a thread past a level's nodes would
/// write a node over the next level's input). The host runs whole groups of the program in
/// its form for that, given the count: every thread inside it stores, none past it does. In
/// the plain form the whole last group stores. Run on the host GPU (or on WARP with
/// `VICTORY_WARP=1`: nothing here asks for a wave size).
#[test]
fn a_dispatch_by_thread_counts_runs_no_thread_past_them() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut validator =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
            .ok();
    let mut shaders = Vec::new();
    for (wave64, group) in [(false, 32u32), (true, 64)] {
        let native = numbering(wave64, group);
        let limited = lowered_for(&native, true).expect("lowered for thread counts");
        let plain = lowered_for(&native, false).expect("lowered");
        assert!(
            limited
                .user_data
                .iter()
                .any(|u| u.root == RootType::Other(super::USER_THREAD_LIMIT)),
            "the counts are read"
        );
        if let Some(v) = validator.as_mut() {
            for s in [&limited, &plain] {
                let mut copy = s.dxil.clone();
                v.validate(&mut copy).expect("DXIL validation failed");
            }
        }
        shaders.push((group, limited, plain));
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; not run");
        return;
    };
    for (group, limited, plain) in &shaders {
        let group = *group;
        for threads in [group / 4 + 1, group + group / 2 - 3, 2 * group] {
            let groups = threads.div_ceil(group);
            let run = |shader: &HostShader| -> Vec<u32> {
                let (root, pso) = host
                    .compute_pipeline(shader)
                    .unwrap_or_else(|e| panic!("pipeline: {e}"));
                let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
                for u in &shader.user_data {
                    if u.root == RootType::Other(super::USER_THREAD_LIMIT) {
                        ud[u.index as usize] = [threads, 1, 1][u.dword.min(2) as usize];
                    }
                }
                let out = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(0, Root::Constants(ud)), (1, Root::Output(0))],
                            views: &[View::RawUav(256)],
                            samplers: &[],
                            groups: [groups, 1, 1],
                        },
                    )
                    .unwrap_or_else(|e| panic!("dispatch: {e}"));
                out.into_iter().last().unwrap_or_default()
            };
            let want: Vec<u32> = (0..256)
                .map(|i| if i < threads { i % group + 1 } else { 0 })
                .collect();
            let out = run(limited);
            assert_eq!(
                out[..256],
                want[..],
                "{threads} threads in groups of {group}"
            );
            let whole = run(plain);
            let n = threads as usize;
            assert_eq!(whole[..n], want[..n], "the plain form, inside the count");
            if threads % group != 0 {
                assert_ne!(whole[n], 0, "the plain form runs the whole last group");
            }
        }
    }
    eprintln!("ran: no thread past the counts stored");
}

/// A console wave64 program of 64 threads a group that reads its wave across the halves: each
/// thread stores, at dwords `2 (group * 64 + thread)` and the next of its root UAV, its rank
/// among the lanes in EXEC (`v_mbcnt`) and their count (`s_bcnt1_i32_b64` of EXEC). On a host
/// of 32-lane waves the two host waves of a console wave exchange both.
fn ranking() -> NativeProgram {
    let mut native = numbering(true, 64);
    let words: Vec<u32> = vec![
        0x8F0C_8602, // s_lshl_b32 s12, s2, 6
        0x8801_FF01,
        0x0001_0000, // s_or_b32 s1, s1, 0x10000
        0xBE82_03C1, // s_mov_b32 s2, -1
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, 0x11014fac
        0xBE84_107E, // s_bcnt1_i32_b64 s4, exec
        0xD765_0001,
        0x0001_007E, // v_mbcnt_lo_u32_b32 v1, exec_lo, 0
        0xD766_0001,
        0x0002_027F, // v_mbcnt_hi_u32_b32 v1, exec_hi, v1
        0x4A04_000C, // v_add_nc_u32 v2, s12, v0
        0x3404_0483, // v_lshlrev_b32 v2, 3, v2
        0x7E06_0204, // v_mov_b32 v3, s4
        0xE070_1000,
        0x8000_0102, // buffer_store_dword v1, v2, s[0:3], 0 offen
        0xE070_1004,
        0x8000_0302, // buffer_store_dword v3, v2, s[0:3], 0 offen offset:4
        0xBF81_0000, // s_endpgm
    ];
    native.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    native
}

/// A console wave whose lanes past a dispatch's thread count were never launched counts only
/// the launched ones: its EXEC, and each lane's rank in it. On a host of 32-lane waves the
/// console wave is two host waves that exchange their halves, and in a group cut short at 32
/// lanes or fewer the upper one has no launched thread at all: it gives "no lanes" to every
/// exchange while still meeting the lower half at both group barriers.
#[test]
fn a_wave_cut_short_by_a_thread_count_counts_its_launched_lanes() {
    let native = ranking();
    let limited = lowered_for(&native, true).expect("lowered for thread counts");
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        let mut copy = limited.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; not run");
        return;
    };
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };
    let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: the option and the size describe the same output structure.
    unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut waves).cast(),
            size_of_val(&waves) as u32,
        )
    }.expect("query wave lane counts");
    if !waves.WaveOps.as_bool() || !(waves.WaveLaneCountMin..=waves.WaveLaneCountMax).contains(&32) {
        eprintln!("adapter wave size range {}..{}, skipping wave32 execution", waves.WaveLaneCountMin, waves.WaveLaneCountMax);
        return;
    }
    let (root, pso) = host.compute_pipeline(&limited).expect("wave32 compute pipeline");
    for threads in [6u32, 32, 45, 64 + 33, 128] {
        let mut ud = vec![0u32; limited.user_data_dwords().max(1) as usize];
        for u in &limited.user_data {
            if u.root == RootType::Other(super::USER_THREAD_LIMIT) {
                ud[u.index as usize] = [threads, 1, 1][u.dword.min(2) as usize];
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud)), (1, Root::Output(0))],
                    views: &[View::RawUav(512)],
                    samplers: &[],
                    groups: [threads.div_ceil(64), 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch: {e}"));
        let out = out.into_iter().last().unwrap_or_default();
        let want: Vec<u32> = (0..256u32)
            .flat_map(|i| {
                if i < threads {
                    let group = i / 64;
                    let launched = (threads - group * 64).min(64);
                    [i % 64, launched]
                } else {
                    [0, 0]
                }
            })
            .collect();
        assert_eq!(out[..512], want[..], "{threads} threads");
    }
    eprintln!("ran: the cut-short waves counted their launched lanes");
}

#[test]
fn translate_title_builder_thread_counts() {
    let Some(path) = std::env::var_os("VICTORY_TEST_BVH_DLL") else {
        return;
    };
    let dll = std::fs::read(path).expect("builder DLL");
    let mut validator =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
            .ok();
    let containers = containers(&dll);
    assert!(!containers.is_empty(), "no builder shaders found");
    let mut count = 0;
    for (i, c) in containers.iter().enumerate() {
        if super::translate(c).is_err() {
            continue;
        }
        let mut shader =
            super::translate_thread_limited(c).unwrap_or_else(|e| panic!("builder {i}: {e}"));
        if let Some(v) = &mut validator {
            v.validate(&mut shader.dxil)
                .unwrap_or_else(|e| panic!("builder {i}: {e}"));
        }
        count += 1;
    }
    assert!(count > 0, "no translated builder shaders");
    eprintln!("{count} builder shaders translated for thread counts");
}
