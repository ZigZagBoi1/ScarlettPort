//! SSE4a's `insertq` / `extrq` as SSE2 code: where a CPU without SSE4a (any Intel one) faults
//! on one, the site is replaced at its first fault by a jump to a stub that does the same with
//! instructions every x86-64 CPU has, and jumps back. A trap costs a trip into Windows and back
//! through every vectored handler; the stub costs a few instructions.
//!
//! Exact as the trap emulation ([`super::decode::insertq`] / [`super::decode::extrq`], checked
//! against an AMD CPU): the low quadword as the instruction leaves it, the high one zeroed. Every
//! other register and the flags come out as they went in: the immediate forms use one scratch
//! XMM register (saved on the stack and back) and touch no flags; the register forms, whose
//! shifts are variable, go through general registers, saved with the flags around them.

use super::decode::{extrq, insertq, ConsoleOnly};

/// A small x86-64 assembler for the stubs: the bytes and the address they will run at.
struct Asm {
    code: Vec<u8>,
    base: usize,
}

/// How an instruction names its memory or register operand.
#[derive(Clone, Copy)]
enum Rm {
    /// An XMM (or general) register.
    Reg(u8),
    /// `[rsp + disp8]`.
    Rsp(i8),
    /// `[rip + disp32]` to this absolute address.
    Rip(usize),
}

impl Asm {
    fn at(&self) -> usize {
        self.base + self.code.len()
    }

    /// `[prefix] [REX] 0F op ModRM ...` with `reg` and `rm`, and the immediate byte after.
    fn sse(&mut self, prefix: u8, op: u8, reg: u8, rm: Rm, imm: Option<u8>) -> Option<()> {
        self.code.push(prefix);
        let b = if let Rm::Reg(r) = rm { r >> 3 & 1 } else { 0 };
        let rex = 0x40 | (reg >> 3 & 1) << 2 | b;
        if rex != 0x40 {
            self.code.push(rex);
        }
        self.code.extend([0x0f, op]);
        let reg = (reg & 7) << 3;
        match rm {
            Rm::Reg(r) => self.code.push(0xc0 | reg | r & 7),
            Rm::Rsp(d) => self.code.extend([0x44 | reg, 0x24, d as u8]),
            Rm::Rip(to) => {
                self.code.push(0x05 | reg);
                let end = self.at() + 4 + usize::from(imm.is_some());
                self.code.extend(i32::try_from(to as i64 - end as i64).ok()?.to_le_bytes());
            }
        }
        self.code.extend(imm);
        Some(())
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.code.extend(bytes);
    }

    fn movdqu_load(&mut self, x: u8, rm: Rm) -> Option<()> {
        self.sse(0xf3, 0x6f, x, rm, None)
    }

    fn movdqu_store(&mut self, rm: Rm, x: u8) -> Option<()> {
        self.sse(0xf3, 0x7f, x, rm, None)
    }
}

/// `lea rsp, [rsp + d]` (no flags changed, as `sub` / `add` would).
fn lea_rsp(d: i8) -> [u8; 5] {
    [0x48, 0x8d, 0x64, 0x24, d as u8]
}

/// Pushes and pops around the register forms: the flags and rax, rcx, rdx, r8, r9.
const SAVE: [u8; 8] = [0x9c, 0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51];
const RESTORE: [u8; 8] = [0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58, 0x9d];

/// The stub for `what` at `stub`, which jumps to `back` when done: its bytes and the offset in
/// them it is entered at (constants for `pand`, aligned, come first). `None` for an instruction
/// that is not SSE4a's or out of reach. `stub` is 16-aligned.
pub(super) fn stub(what: ConsoleOnly, stub: usize, back: usize) -> Option<(Vec<u8>, usize)> {
    debug_assert_eq!(stub & 15, 0);
    let mut a = Asm { code: Vec::new(), base: stub };
    let entry;
    match what {
        ConsoleOnly::Insertq { dst, src, reg: false, len, idx } => {
            let (d, s) = (dst & 15, src & 15);
            // The field's mask and the bits kept, each with a zero high quadword.
            let mask = insertq(0, u64::MAX, len, idx);
            for q in [mask, 0, !mask, 0] {
                a.raw(&q.to_le_bytes());
            }
            entry = a.code.len();
            let t = (0..16).find(|&t| t != d && t != s)?;
            a.raw(&lea_rsp(-16));
            a.movdqu_store(Rm::Rsp(0), t)?;
            a.sse(0xf3, 0x7e, t, Rm::Reg(s), None)?; // movq t, s (the high quadword zeroed)
            a.sse(0x66, 0x73, 6, Rm::Reg(t), Some(idx & 63))?; // psllq t, idx
            a.sse(0x66, 0xdb, t, Rm::Rip(stub), None)?; // pand t, [mask]
            a.sse(0x66, 0xdb, d, Rm::Rip(stub + 16), None)?; // pand d, [!mask]
            a.sse(0x66, 0xeb, d, Rm::Reg(t), None)?; // por d, t
            a.movdqu_load(t, Rm::Rsp(0))?;
            a.raw(&lea_rsp(16));
        }
        ConsoleOnly::Extrq { dst, src: None, len, idx } => {
            let d = dst & 15;
            for q in [extrq(u64::MAX, len, 0), 0] {
                a.raw(&q.to_le_bytes());
            }
            entry = a.code.len();
            a.sse(0x66, 0x73, 2, Rm::Reg(d), Some(idx & 63))?; // psrlq d, idx
            a.sse(0x66, 0xdb, d, Rm::Rip(stub), None)?; // pand d, [ones]
        }
        ConsoleOnly::Insertq { dst, src, reg: true, .. } | ConsoleOnly::Extrq { dst, src: Some(src), .. } => {
            let insert = matches!(what, ConsoleOnly::Insertq { .. });
            let (d, s) = (dst & 15, src & 15);
            entry = 0;
            a.raw(&SAVE);
            a.raw(&lea_rsp(-32));
            a.movdqu_store(Rm::Rsp(0), d)?;
            a.movdqu_store(Rm::Rsp(16), s)?;
            a.raw(&[0x48, 0x8b, 0x44, 0x24, 0x00]); // mov rax, [rsp] (the destination)
            if insert {
                a.raw(&[0x48, 0x8b, 0x54, 0x24, 0x10]); // mov rdx, [rsp+16] (the source)
                a.raw(&[0x48, 0x8b, 0x4c, 0x24, 0x18]); // mov rcx, [rsp+24] (length, index)
            } else {
                a.raw(&[0x48, 0x8b, 0x4c, 0x24, 0x10]); // mov rcx, [rsp+16] (length, index)
            }
            a.raw(&[0x49, 0x89, 0xc8]); // mov r8, rcx
            a.raw(&[0x49, 0xc1, 0xe8, 0x08]); // shr r8, 8 (the index in r8b)
            // ones = -1 >> (64 - len) mod 64: a length of 0 is 64
            a.raw(&[0xf7, 0xd9]); // neg ecx
            a.raw(&[0x49, 0xc7, 0xc1, 0xff, 0xff, 0xff, 0xff]); // mov r9, -1
            a.raw(&[0x49, 0xd3, 0xe9]); // shr r9, cl
            a.raw(&[0x44, 0x89, 0xc1]); // mov ecx, r8d
            if insert {
                a.raw(&[0x49, 0xd3, 0xe1]); // shl r9, cl (the mask)
                a.raw(&[0x48, 0xd3, 0xe2]); // shl rdx, cl
                a.raw(&[0x4c, 0x21, 0xca]); // and rdx, r9
                a.raw(&[0x49, 0xf7, 0xd1]); // not r9
                a.raw(&[0x4c, 0x21, 0xc8]); // and rax, r9
                a.raw(&[0x48, 0x09, 0xd0]); // or rax, rdx
            } else {
                a.raw(&[0x48, 0xd3, 0xe8]); // shr rax, cl
                a.raw(&[0x4c, 0x21, 0xc8]); // and rax, r9
            }
            a.raw(&[0x48, 0x89, 0x44, 0x24, 0x00]); // mov [rsp], rax
            a.raw(&[0x48, 0xc7, 0x44, 0x24, 0x08, 0, 0, 0, 0]); // mov qword [rsp+8], 0
            a.movdqu_load(d, Rm::Rsp(0))?;
            a.raw(&lea_rsp(32));
            a.raw(&RESTORE);
        }
        _ => return None,
    }
    let from = a.at();
    a.raw(&super::jump(from, back)?);
    Some((a.code, entry))
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use core::arch::asm;
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};

    fn sse4a() -> bool {
        let max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        max >= 0x8000_0001 && core::arch::x86_64::__cpuid(0x8000_0001).ecx & (1 << 6) != 0
    }

    /// A page of our own: a site at 0 (filled per run), its return at 16, stubs from 64.
    struct Page(usize);
    impl Page {
        fn new() -> Page {
            // SAFETY: fresh memory or null.
            let p = unsafe { VirtualAlloc(std::ptr::null(), 1 << 16, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as usize;
            assert_ne!(p, 0);
            Page(p)
        }
        fn put(&self, off: usize, bytes: &[u8]) {
            // SAFETY: inside the page.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (self.0 + off) as *mut u8, bytes.len()) };
        }
    }
    impl Drop for Page {
        fn drop(&mut self) {
            // SAFETY: ours.
            unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
        }
    }

    const BACK: usize = 16;
    const STUB: usize = 64;

    /// All sixteen XMM registers, the five general ones the stubs use and the flags, through a
    /// call to `site`: what came out.
    fn run(site: usize, x: &mut [[u64; 2]; 16], flags_in: u64) -> ([u64; 5], u64, usize) {
        let mut g = [0x1111_u64, 0x2222, 0x3333, 0x4444, 0x5555];
        let mut f = flags_in;
        let mut sp = 0usize;
        // SAFETY: `site` is code that returns with every register but the XMM ones as it was.
        unsafe {
            asm!(
                "mov {sp}, rsp",
                "movdqu xmm0, [{x}]", "movdqu xmm1, [{x}+16]", "movdqu xmm2, [{x}+32]", "movdqu xmm3, [{x}+48]",
                "movdqu xmm4, [{x}+64]", "movdqu xmm5, [{x}+80]", "movdqu xmm6, [{x}+96]", "movdqu xmm7, [{x}+112]",
                "movdqu xmm8, [{x}+128]", "movdqu xmm9, [{x}+144]", "movdqu xmm10, [{x}+160]", "movdqu xmm11, [{x}+176]",
                "movdqu xmm12, [{x}+192]", "movdqu xmm13, [{x}+208]", "movdqu xmm14, [{x}+224]", "movdqu xmm15, [{x}+240]",
                "push {f}", "popfq",
                "call {site}",
                "pushfq", "pop {f}", "cld",
                "sub {sp}, rsp",
                "movdqu [{x}], xmm0", "movdqu [{x}+16], xmm1", "movdqu [{x}+32], xmm2", "movdqu [{x}+48], xmm3",
                "movdqu [{x}+64], xmm4", "movdqu [{x}+80], xmm5", "movdqu [{x}+96], xmm6", "movdqu [{x}+112], xmm7",
                "movdqu [{x}+128], xmm8", "movdqu [{x}+144], xmm9", "movdqu [{x}+160], xmm10", "movdqu [{x}+176], xmm11",
                "movdqu [{x}+192], xmm12", "movdqu [{x}+208], xmm13", "movdqu [{x}+224], xmm14", "movdqu [{x}+240], xmm15",
                x = in(reg) x.as_mut_ptr(), site = in(reg) site, f = inout(reg) f, sp = out(reg) sp,
                inout("rax") g[0], inout("rcx") g[1], inout("rdx") g[2], inout("r8") g[3], inout("r9") g[4],
                out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
                out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _, out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            );
        }
        (g, f, sp)
    }

    fn state(seed: u64) -> [[u64; 2]; 16] {
        let mut x = [[0u64; 2]; 16];
        let mut s = seed | 1;
        for r in &mut x {
            for q in r.iter_mut() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                *q = s;
            }
        }
        x
    }

    /// The instruction's bytes, as the title has them.
    fn encode(what: ConsoleOnly) -> Vec<u8> {
        let (p, op, reg, rm, imm) = match what {
            ConsoleOnly::Insertq { dst, src, reg: false, len, idx } => (0xf2, 0x78, dst, src, Some([len, idx])),
            ConsoleOnly::Insertq { dst, src, reg: true, .. } => (0xf2, 0x79, dst, src, None),
            ConsoleOnly::Extrq { dst, src: None, len, idx } => (0x66, 0x78, 0, dst, Some([len, idx])),
            ConsoleOnly::Extrq { dst, src: Some(src), .. } => (0x66, 0x79, dst, src, None),
            _ => unreachable!(),
        };
        let mut b = vec![p];
        let rex = 0x40 | (reg >> 3) << 2 | rm >> 3;
        if rex != 0x40 {
            b.push(rex);
        }
        b.extend([0x0f, op, 0xc0 | (reg & 7) << 3 | rm & 7]);
        b.extend(imm.into_iter().flatten());
        assert_eq!(super::super::decode::decode(&b), Some((what, b.len())), "{b:02x?}");
        b
    }

    /// Runs `what` as the CPU does (when it has SSE4a) and as the stub does, from the same
    /// registers and flags: the same registers, flags and stack pointer come out.
    fn check(page: &Page, what: ConsoleOnly, x0: &[[u64; 2]; 16], hw: bool) {
        let (body, entry) = stub(what, page.0 + STUB, page.0 + BACK).unwrap();
        page.put(STUB, &body);
        page.put(0, &crate::console_insn::jump(page.0, page.0 + STUB + entry).unwrap());
        page.put(BACK, &[0xc3]);
        for flags in [0x202u64, 0xed7] {
            let mut x = *x0;
            let (g, f, sp) = run(page.0, &mut x, flags);
            assert_eq!(g, [0x1111, 0x2222, 0x3333, 0x4444, 0x5555], "{what:?}: general registers");
            assert_eq!(f & 0xcd5, flags & 0xcd5, "{what:?}: flags");
            assert_eq!(sp, 0, "{what:?}: stack");
            let want = if hw {
                let insn = encode(what);
                let mut y = *x0;
                page.put(32, &insn);
                page.put(32 + insn.len(), &[0xc3]);
                run(page.0 + 32, &mut y, flags);
                y
            } else {
                let mut y = *x0;
                emulate(what, &mut y);
                y
            };
            assert_eq!(x, want, "{what:?}");
        }
    }

    /// The trap emulation's arithmetic on a register file.
    fn emulate(what: ConsoleOnly, x: &mut [[u64; 2]; 16]) {
        match what {
            ConsoleOnly::Insertq { dst, src, reg, len, idx } => {
                let s = x[src as usize];
                let (len, idx) = if reg { (s[1] as u8, (s[1] >> 8) as u8) } else { (len, idx) };
                x[dst as usize] = [insertq(x[dst as usize][0], s[0], len, idx), 0];
            }
            ConsoleOnly::Extrq { dst, src, len, idx } => {
                let (len, idx) = match src {
                    Some(s) => (x[s as usize][0] as u8, (x[s as usize][0] >> 8) as u8),
                    None => (len, idx),
                };
                x[dst as usize] = [extrq(x[dst as usize][0], len, idx), 0];
            }
            _ => unreachable!(),
        }
    }

    fn defined(len: u8, idx: u8) -> bool {
        !(len != 0 && len as u32 + idx as u32 > 64 || len == 0 && idx != 0)
    }

    /// Every length and index, every pair of XMM registers, both forms of both instructions,
    /// against the CPU (an AMD one, which has them) or else the trap emulation's arithmetic.
    #[test]
    fn the_stubs_do_what_the_instructions_do() {
        let hw = sse4a();
        if !hw {
            eprintln!("no SSE4a on this CPU: the stubs are checked against the emulation's arithmetic");
        }
        let page = Page::new();
        let x0 = state(0x9e37_79b9_7f4a_7c15);
        let mut n = 0u64;
        for len in 0u8..64 {
            for idx in 0u8..64 {
                if !defined(len, idx) {
                    continue;
                }
                for d in 0u8..16 {
                    check(&page, ConsoleOnly::Extrq { dst: d, src: None, len, idx }, &x0, hw);
                    for s in 0u8..16 {
                        check(&page, ConsoleOnly::Insertq { dst: d, src: s, reg: false, len, idx }, &x0, hw);
                        n += 1;
                    }
                }
            }
        }
        // The register forms: length and index from the source, every pair, every field.
        for d in 0u8..16 {
            for s in 0u8..16 {
                for (len, idx) in [(0u8, 0u8), (16, 16), (1, 63), (63, 1), (5, 59), (32, 32), (12, 20)] {
                    let mut x = x0;
                    x[s as usize][1] = x[s as usize][1] & !0xffff | u64::from(len) | u64::from(idx) << 8;
                    check(&page, ConsoleOnly::Insertq { dst: d, src: s, reg: true, len: 0, idx: 0 }, &x, hw);
                    let mut x = x0;
                    x[s as usize][0] = x[s as usize][0] & !0xffff | u64::from(len) | u64::from(idx) << 8;
                    check(&page, ConsoleOnly::Extrq { dst: d, src: Some(s), len: 0, idx: 0 }, &x, hw);
                }
            }
        }
        for len in 0u8..64 {
            for idx in 0u8..64 {
                if defined(len, idx) {
                    let mut x = x0;
                    x[1][1] = u64::from(len) | u64::from(idx) << 8 | 0xabcd_0000;
                    check(&page, ConsoleOnly::Insertq { dst: 0, src: 1, reg: true, len: 0, idx: 0 }, &x, hw);
                    let mut x = x0;
                    x[3][0] = u64::from(len) | u64::from(idx) << 8 | 0xabcd_0000;
                    check(&page, ConsoleOnly::Extrq { dst: 7, src: Some(3), len: 0, idx: 0 }, &x, hw);
                }
            }
        }
        eprintln!("{n} immediate insertq forms checked (hardware: {hw})");
    }

    /// The jump at the site reaches the stub's entry, and the stub's own jump the instruction
    /// after the site.
    #[test]
    fn the_jumps_go_where_they_should() {
        let site = 0x1_4000_0000usize + 0x2e7ff1;
        let stub_at = 0x1_3ff0_0000usize;
        let what = ConsoleOnly::Insertq { dst: 0, src: 1, reg: false, len: 0x10, idx: 0x10 };
        let (body, entry) = stub(what, stub_at, site + 6).unwrap();
        let j = crate::console_insn::jump(site, stub_at + entry).unwrap();
        assert_eq!(j[0], 0xe9);
        let rel = i32::from_le_bytes(j[1..5].try_into().unwrap()) as i64;
        assert_eq!((site as i64 + 5 + rel) as usize, stub_at + entry);
        let end = body.len();
        assert_eq!(body[end - 5], 0xe9);
        let rel = i32::from_le_bytes(body[end - 4..].try_into().unwrap()) as i64;
        assert_eq!((stub_at as i64 + end as i64 + rel) as usize, site + 6, "back to the next instruction");
        assert!(stub(what, stub_at, stub_at + (3 << 30)).is_none(), "out of reach");
        assert!(stub(ConsoleOnly::Clzero, stub_at, site).is_none());
    }
}
