//! `SCARLETT_WAVE64_LDS` (on unless `0`) ([`lower::wave64_lds`]): the pattern that kept 29 console wave64
//! compute programs of the title at 64-lane waves on a host of 32 – LDS written and read back
//! by other lanes inside one per-lane branch, with no barrier between (the console runs the
//! wave's lanes in step) – encoded as a synthetic GFX10 program, lowered with the switch off
//! and on, in groups of one console wave and of two, validated by `dxil.dll` when present
//! and run on WARP (the CPU device, never the card) when it takes the program.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const V: u32 = 256;
const S_ENDPGM: u32 = 0xBF81_0000;

/// VOP3 `op vdst, src0, src1, src2` (`src`: 0..105 SGPR, 128.. inline, 256.. VGPR).
const fn vop3(op: u32, vdst: u32, src0: u32, src1: u32, src2: u32) -> [u32; 2] {
    [0x35 << 26 | op << 16 | vdst, src0 | src1 << 9 | src2 << 18]
}

/// A compute program of `threads` threads (thread index in v0) with 4 root constants in
/// s0..s3, a raw UAV's descriptor in s[8:11] and `lds_bytes` of LDS, then `body`.
fn native(body: &[u32], wave64: bool, threads: u32, lds_bytes: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: (5 << 1) | ((lds_bytes / 512) << 15), // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// DS `op vdst, addr, data0` (offset 0).
const fn ds(op: u32, vdst: u32, addr: u32, data0: u32) -> [u32; 2] {
    [0x36 << 26 | op << 18, vdst << 24 | data0 << 8 | addr]
}

/// `buffer_store_dword v<vdata>, v<vaddr>, s[8:11], 0 offen`.
const fn store_at(vdata: u32, vaddr: u32) -> [u32; 2] {
    [0x38 << 26 | 0x1C << 18 | 1 << 12, 0x80 << 24 | 2 << 16 | vdata << 8 | vaddr]
}

/// Thread t (v0) of the group: `if (t < s0) { lds[t] = t + s2 + r; out[t] = lds[t ^ 32]; }`,
/// `out[t] = ~0` elsewhere, where r is lane 37's t (`v_readlane_b32`, before the branch) in
/// a group of several console waves, as the title's have (a group of several console waves
/// with no cross-lane code is not held to the console's waves at all), and 0 in a group of
/// one. The partner `t ^ 32` is the same console lane in the other half of the wave: on the
/// console its write is seen (one wave, lanes in step); as two host waves, only if the
/// halves meet between the write and the read. s1 holds 128 (`4 * 32`).
fn pattern(threads: u32) -> NativeProgram {
    let mut words = vec![
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0         (4t)
        0x7E06_02C1, // v_mov_b32 v3, -1
    ];
    words.extend(store_at(3, 1)); //                      out[t] = ~0
    words.extend(vop3(0x125, 4, V, 2, 0)); // v_add_nc_u32 v4, v0, s2
    if threads > 64 {
        words.extend(vop3(0x360, 7, V, 128 + 37, 0)); // v_readlane_b32 s7, v0, 37
        words.extend(vop3(0x125, 4, V + 4, 7, 0)); // v_add_nc_u32 v4, v4, s7
    }
    words.extend([
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | 9, // s_cbranch_execz past the body
    ]);
    words.extend(ds(13, 0, 1, 4)); // ds_write_b32 v1, v4          lds[t] = t + s2
    words.extend(vop3(0x11D, 2, 1, V + 1, 0)); // v_xor_b32 v2, s1, v1   4 (t ^ 32)
    words.extend(ds(54, 3, 2, 0)); // ds_read_b32 v3, v2           lds[t ^ 32]
    words.push(0xBF8C_C07F); // s_waitcnt lgkmcnt(0)
    words.extend(store_at(3, 1)); //                      out[t]
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    native(&words, true, threads, 1024)
}

/// `native` lowered for a host of 32-lane waves with `wide` branches taken wave-wide, the
/// switch as given.
fn lowered(native: &NativeProgram, wide: &HashSet<usize>, on: bool) -> Result<lower::Lowered, crate::ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure_wide(&p, wide).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was_wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let was_on = lower::TEST_WAVE64_LDS.replace(Some(on));
    let r = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was_wave);
    lower::TEST_WAVE64_LDS.set(was_on);
    r
}

/// As the emit ladder's `widen` does it: lowered, the branches around the blocks the lowering
/// names taken wave-wide, until it no longer asks (at most 4 rounds).
fn widened(native: &NativeProgram, on: bool) -> (lower::Lowered, HashSet<usize>) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let mut wide = HashSet::new();
    for _ in 0..4 {
        lower::take_needs();
        lower::take_lds_region();
        let r = lowered(native, &wide, on).expect("lower");
        let needs = lower::take_needs();
        if r.wave_size != Some(64) || needs.is_empty() {
            return (r, wide);
        }
        let st = structure::structure_wide(&p, &wide).expect("structure");
        let more: Vec<usize> = lower::branches_around(&st, &needs).unwrap_or_default();
        if more.iter().all(|b| wide.contains(b)) {
            return (r, wide);
        }
        wide.extend(more);
    }
    panic!("widening did not settle");
}

fn validate(dxil: &[u8], what: &str) {
    match validator::Validator::load(validator::Validator::default_path().as_deref()) {
        Ok(mut v) => {
            let mut copy = dxil.to_vec();
            v.validate(&mut copy).unwrap_or_else(|e| panic!("{what}: DXIL validation failed: {e}"));
        }
        Err(e) => eprintln!("{what}: no dxil.dll ({e}); not validated"),
    }
}

#[test]
fn lds_shared_in_a_lane_branch_keeps_64_lanes_with_the_switch_off() {
    for threads in [64, 128] {
        let native = pattern(threads);
        lower::take_lds_region();
        let r = lowered(&native, &HashSet::new(), false).expect("lower");
        assert_eq!(r.wave_size, Some(64), "{threads} threads");
        assert!(lower::take_lds_region(), "the pattern is named ({threads} threads)");
    }
}

#[test]
fn lds_shared_in_a_lane_branch_runs_as_two_halves_with_the_switch_on() {
    for threads in [64, 128] {
        let native = pattern(threads);
        let (r, wide) = widened(&native, true);
        assert_eq!(r.wave_size, Some(32), "{threads} threads: waves of 32");
        assert_eq!(wide.len(), 1, "{threads} threads: the one lane branch, wave-wide");
        validate(&r.dxil, &format!("{threads} threads"));
        // in a group of two console waves the halves meet on their own (counters, fences):
        // not the group barriers the same branches give without the switch, which would wait
        // for the other console wave
        let group = lowered(&native, &wide, false).expect("lower");
        if threads > 64 {
            assert_ne!(r.dxil, group.dxil, "pair meetings in place of group barriers");
        } else {
            assert_eq!(r.dxil, group.dxil, "one console wave: the group is the pair");
        }
        eprintln!("{threads} threads: {} bytes", r.dxil.len());
    }
}

#[test]
fn off_the_switch_leaves_other_lowerings_alone() {
    // a group of one console wave widened under the switch is the existing wave-wide form,
    // the same module as without it
    let native = pattern(64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let (on, wide) = widened(&native, true);
    let off = lowered(&native, &wide, false).expect("lower");
    assert_eq!(on.dxil, off.dxil);
    // and a program without the pattern is the same either way
    let plain = native_plain();
    let a = lowered(&plain, &HashSet::new(), false).expect("lower");
    let b = lowered(&plain, &HashSet::new(), true).expect("lower");
    assert_eq!(a.dxil, b.dxil);
    let _ = p;
}

/// The LDS write and read in lanes every thread runs (no branch): no pattern.
fn native_plain() -> NativeProgram {
    let mut words = vec![0x3402_0082]; // v_lshlrev_b32 v1, 2, v0
    words.extend(ds(13, 0, 1, 0));
    words.extend(vop3(0x11D, 2, 1, V + 1, 0));
    words.extend(ds(54, 3, 2, 0));
    words.extend(store_at(3, 1));
    native(&words, true, 128, 1024)
}

/// The console's result of [`pattern`]: `out[t] = (t ^ 32) + s2 + r` below s0 when the
/// partner is below s0 too (else what LDS held: not compared), `~0` from the others.
fn expected(threads: u32, below: u32, add: u32, t: u32) -> Option<u32> {
    if t >= below {
        return Some(u32::MAX);
    }
    let partner = t ^ 32;
    let r = if threads > 64 { t / 64 * 64 + 37 } else { 0 };
    (partner < below).then_some(partner + add + r)
}

#[test]
fn lds_shared_in_a_lane_branch_gives_the_consoles_result_on_warp() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else {
        eprintln!("no SM 6.6 WARP device; not run");
        return;
    };
    for threads in [64u32, 128] {
        let native = pattern(threads);
        let (r, _) = widened(&native, true);
        let shader = HostShader {
            stage: HwStage::Cs,
            dxil: r.dxil,
            user_data: r.user_data,
            samplers: r.samplers,
            static_samplers: Vec::new(),
            root_cbvs: r.root_cbvs,
            root_srvs: r.root_srvs,
            root_uavs: r.root_uavs,
            root_indexed_views: Vec::new(),
        };
        let (root, pso) = match host.compute_pipeline(&shader) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("WARP does not take the waves-of-32 program: {e}; not run");
                return;
            }
        };
        for below in [threads, 40, 33, 96.min(threads), 10] {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                if let (RootType::Constants, 0, d @ 0..=2) = (u.root, u.slot, u.dword) {
                    ud[u.index as usize] = [below, 128, 1000][d as usize];
                }
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(threads)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
            for t in 0..threads {
                if let Some(want) = expected(threads, below, 1000, t) {
                    assert_eq!(out[0][t as usize], want, "{threads} threads, {below} below: thread {t}");
                }
            }
        }
        eprintln!("ran on WARP: {threads} threads, the console's result in every case");
    }
}
