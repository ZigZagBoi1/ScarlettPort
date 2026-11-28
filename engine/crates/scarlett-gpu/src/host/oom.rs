//! The host's D3D12 objects refused for want of memory (`E_OUTOFMEMORY`).
//!
//! Players' runs ended in "a host command list did not close (... 0x8007000E):
//! a command recorded into it was invalid" and "no host list for a state
//! prelude", each pointing at `SCARLETT_D3D12_DEBUG` as if a command were bad
//! (issues #530, #547, #568, #574, #575, #580, #592, #603). It was memory:
//! the RAM, page file or the PC's memory shared with the card had run out.
//! An object the host keeps idle (host lists that have run, spare scratch
//! chunks) is let go and the object made once more; the run ends only if that
//! is refused too, and then says what ran out.

use windows::Win32::Foundation::E_OUTOFMEMORY;

/// Whether D3D12 refused something for want of memory.
pub(crate) fn out_of_memory(error: &windows::core::Error) -> bool {
    error.code() == E_OUTOFMEMORY
}

/// `make`, and once more after `free` if it was refused for want of memory.
pub(crate) fn once_more<T>(
    mut make: impl FnMut() -> windows::core::Result<T>,
    free: impl FnOnce(),
) -> windows::core::Result<T> {
    match make() {
        Err(e) if out_of_memory(&e) => {
            free();
            make()
        }
        made => made,
    }
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// What the run's end says for `what` refused for want of memory (`again`:
/// after what was idle was let go): Windows' commit (`used`, `limit`, free
/// RAM; `None` if it could not be read) and the card's memory groups (own,
/// shared with the PC: budget, in use).
pub(crate) fn text(what: &str, again: bool, commit: Option<(u64, u64, u64)>, card: Option<[(u64, u64); 2]>) -> String {
    let mut t = format!("{what}: out of memory (0x8007000E), not a bad command");
    if again {
        t.push_str("; idle host lists and scratch were let go and it was refused again");
    }
    if let Some((used, limit, free)) = commit {
        t.push_str(&format!(
            "; RAM + page file: {:.1} of {:.1} GB committed, {:.1} GB of RAM free",
            gb(used),
            gb(limit),
            gb(free)
        ));
    }
    if let Some([own, shared]) = card {
        t.push_str(&format!(
            "; the card's memory: {} of {} MB in use; the PC's memory shared with the card: {} of {} MB",
            own.1 >> 20,
            own.0 >> 20,
            shared.1 >> 20,
            shared.0 >> 20
        ));
    }
    t.push_str(
        "; close other programs, lower the game's settings, or set a bigger page file (README)",
    );
    t
}

/// Windows' commit as [`text`] takes it.
pub(crate) fn commit() -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0)
        .then(|| (m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile), m.ullTotalPageFile, m.ullAvailPhys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};

    #[test]
    fn only_a_refusal_for_memory_is_tried_once_more() {
        // Refused for memory, then made: one free, two tries.
        let (mut tries, mut freed) = (0, 0);
        let made = once_more(
            || {
                tries += 1;
                if tries == 1 { Err(E_OUTOFMEMORY.into()) } else { Ok(7) }
            },
            || freed += 1,
        );
        assert_eq!((made.unwrap(), tries, freed), (7, 2, 1));
        // Refused for memory twice: the second refusal is what comes back.
        let mut tries = 0;
        let made: windows::core::Result<()> = once_more(
            || {
                tries += 1;
                Err(E_OUTOFMEMORY.into())
            },
            || {},
        );
        assert!(out_of_memory(&made.unwrap_err()));
        assert_eq!(tries, 2);
        // Any other error, or a success: one try, nothing let go.
        for first in [Err(E_INVALIDARG.into()), Ok(1)] {
            let (mut tries, mut freed) = (0, 0);
            let mut first = Some(first);
            let made = once_more(
                || {
                    tries += 1;
                    first.take().unwrap()
                },
                || freed += 1,
            );
            assert_eq!((tries, freed), (1, 0));
            assert!(made.map_or_else(|e| e.code() == E_INVALIDARG, |v| v == 1));
        }
    }

    #[test]
    fn the_end_says_memory_ran_out_with_the_numbers() {
        const MB: u64 = 1 << 20;
        let t = text(
            "a host command list did not close",
            false,
            Some((15 * 1024 * MB + 512 * MB, 16 * 1024 * MB, 300 * MB)),
            Some([(15420 * MB, 8689 * MB), (7096 * MB, 7240 * MB)]),
        );
        assert!(t.starts_with("a host command list did not close: out of memory (0x8007000E), not a bad command"), "{t}");
        assert!(t.contains("RAM + page file: 15.5 of 16.0 GB committed, 0.3 GB of RAM free"), "{t}");
        assert!(t.contains("the card's memory: 8689 of 15420 MB in use; the PC's memory shared with the card: 7240 of 7096 MB"), "{t}");
        assert!(t.ends_with("set a bigger page file (README)"), "{t}");
        assert!(!t.contains("SCARLETT_D3D12_DEBUG") && !t.contains("invalid") && !t.contains("let go"), "{t}");
        // Numbers that could not be read are left out.
        let t = text("no host list for a state prelude", true, None, None);
        assert!(!t.contains("GB") && !t.contains("MB"), "{t}");
        assert!(t.contains("not a bad command; idle host lists and scratch were let go and it was refused again"), "{t}");
        assert!(t.contains("not a bad command") && t.contains("page file (README)"), "{t}");
    }
}
