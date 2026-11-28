//! Which card the host device is made on (#274, #577).
//!
//! Windows' high-performance order alone was the choice (#274): right on an
//! Optimus laptop, where the default adapter is the iGPU, and a guess on a PC
//! with two cards of its own, where both count as high performance and the
//! order says nothing of which one the monitor is on. A PC with an RTX 5060 Ti
//! and an RTX 3070 had its device made on the 3070, the card it does not use,
//! and the device was removed (#577).
//!
//! The choice now:
//!
//! - `SCARLETT_ADAPTER=<n>`: the n-th hardware adapter of the list the log
//!   prints (0 is the first); a number past the end of the list is a piece
//!   of a name, so `SCARLETT_ADAPTER=5060` takes the RTX 5060 Ti;
//! - `SCARLETT_ADAPTER=<a piece of the name>`: the adapters whose name holds
//!   it, any case (`nvidia`, `rtx 3070`);
//! - otherwise, the one with the most memory of its own (counted in whole
//!   GiB, so two 8 GB cards are alike); of those, one a monitor is connected
//!   to; of those, the first in Windows' order.
//!
//! An iGPU has little memory of its own, so a laptop still gets its
//! discrete card. If the device cannot be made on the one chosen, the next
//! is tried, in the same order.

use windows::Win32::Graphics::Dxgi::*;

/// The variable that names the card.
pub(crate) const ENV: &str = "SCARLETT_ADAPTER";

/// What the log and the choice need of one hardware adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) name: String,
    /// The adapter's own video memory, in bytes (`DedicatedVideoMemory`).
    pub(crate) memory: u64,
    /// Whether a monitor is connected to it (it has an output).
    pub(crate) display: bool,
}

/// Why the first of [`Choice::order`] comes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// `SCARLETT_ADAPTER` names it.
    Asked,
    /// It is the only hardware adapter.
    Only,
    /// It is Windows' first by high performance, and no other has more
    /// memory, or as much and a monitor where it has none.
    First,
    /// It has more memory of its own than Windows' first.
    Memory,
    /// It has as much memory as Windows' first and a monitor, where that
    /// one has none.
    Display,
}

impl Why {
    pub(crate) fn says(self) -> &'static str {
        match self {
            Why::Asked => "SCARLETT_ADAPTER names it",
            Why::Only => "the only card there is",
            Why::First => "Windows' first by high performance",
            Why::Memory => "it has more memory of its own than Windows' first by high performance",
            Why::Display => {
                "a monitor is connected to it and not to Windows' first by high performance, which has as much memory"
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    /// Indices into the adapters seen: the one to make the device on first,
    /// then the rest, in the order to try them if it fails.
    pub(crate) order: Vec<usize>,
    pub(crate) why: Why,
    /// `SCARLETT_ADAPTER` was set and names none of them.
    pub(crate) unmatched: bool,
}

/// Memory in whole GiB, the nearest, so cards sold with the same amount
/// (which report a few MB apart) count as alike.
fn gib(bytes: u64) -> u64 {
    (bytes + (1 << 29)) >> 30
}

/// Whether `name` holds `digits` as a whole number: `5060` is in
/// "RTX 5060 Ti" and `2` is not in "RTX 2080".
fn holds_number(name: &str, digits: &str) -> bool {
    name.match_indices(digits).any(|(at, _)| {
        let before = name[..at].chars().next_back();
        let after = name[at + digits.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// The adapters `asked` names: an index into `seen`, or else a piece of
/// their names (a number past the end of the list, only as a whole number).
fn named(seen: &[Seen], asked: &str) -> Vec<usize> {
    let asked = asked.trim().to_lowercase();
    if asked.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(i) = asked.parse::<usize>()
            && i < seen.len()
        {
            return vec![i];
        }
        return (0..seen.len())
            .filter(|&i| holds_number(&seen[i].name, &asked))
            .collect();
    }
    (0..seen.len())
        .filter(|&i| seen[i].name.to_lowercase().contains(&asked))
        .collect()
}

/// The order to try `seen` (hardware adapters, in Windows' high-performance
/// order) in, and why the first comes first. `asked` is `SCARLETT_ADAPTER`.
pub(crate) fn choose(seen: &[Seen], asked: Option<&str>) -> Choice {
    // Best first: more memory, then a monitor, then Windows' order (the
    // sort keeps that order among equals).
    let mut ranked: Vec<usize> = (0..seen.len()).collect();
    ranked.sort_by_key(|&i| (std::cmp::Reverse(gib(seen[i].memory)), !seen[i].display));
    let asked = asked.map(str::trim).filter(|a| !a.is_empty());
    let picked = asked.map(|a| named(seen, a)).unwrap_or_default();
    if !picked.is_empty() {
        let (mut order, rest): (Vec<usize>, Vec<usize>) =
            ranked.into_iter().partition(|i| picked.contains(i));
        order.extend(rest);
        return Choice {
            order,
            why: Why::Asked,
            unmatched: false,
        };
    }
    let why = match ranked.first() {
        _ if seen.len() <= 1 => Why::Only,
        Some(0) | None => Why::First,
        Some(&i) if gib(seen[i].memory) > gib(seen[0].memory) => Why::Memory,
        Some(_) => Why::Display,
    };
    Choice {
        order: ranked,
        why,
        unmatched: asked.is_some(),
    }
}

/// The hardware adapters (the software one left out) in Windows'
/// high-performance order, with what [`choose`] needs of each.
pub(crate) fn hardware() -> Vec<(IDXGIAdapter1, Seen)> {
    let mut out = Vec::new();
    // SAFETY: plain DXGI factory and adapter queries.
    unsafe {
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory
            .EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
        {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let n = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let seen = Seen {
                name: String::from_utf16_lossy(&desc.Description[..n]),
                memory: desc.DedicatedVideoMemory as u64,
                display: adapter.EnumOutputs(0).is_ok(),
            };
            out.push((adapter, seen));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Seen, Why, choose};

    const GB: u64 = 1 << 30;

    fn card(name: &str, memory: u64, display: bool) -> Seen {
        Seen {
            name: name.into(),
            memory,
            display,
        }
    }

    #[test]
    fn one_card_is_the_only_choice() {
        let c = choose(&[card("AMD Radeon (TM) Graphics", 512 << 20, true)], None);
        assert_eq!((c.order, c.why, c.unmatched), (vec![0], Why::Only, false));
    }

    #[test]
    fn a_laptop_keeps_its_discrete_card() {
        // Optimus (#274): Windows ranks the card first, the iGPU drives the screen.
        let seen = [
            card(
                "NVIDIA GeForce RTX 4060 Laptop GPU",
                8 * GB - (200 << 20),
                false,
            ),
            card("Intel(R) UHD Graphics", 128 << 20, true),
        ];
        let c = choose(&seen, None);
        assert_eq!((c.order, c.why), (vec![0, 1], Why::First));
    }

    #[test]
    fn more_memory_wins_over_windows_order() {
        // #577 with a 16 GB 5060 Ti: Windows ranked the 3070 first.
        let seen = [
            card("NVIDIA GeForce RTX 3070", 8 * GB - (190 << 20), false),
            card("NVIDIA GeForce RTX 5060 Ti", 16 * GB - (300 << 20), true),
        ];
        let c = choose(&seen, None);
        assert_eq!((c.order, c.why), (vec![1, 0], Why::Memory));
    }

    #[test]
    fn equal_memory_goes_to_the_card_with_the_monitor() {
        // #577 with an 8 GB 5060 Ti: the same memory, a few MB apart.
        let seen = [
            card("NVIDIA GeForce RTX 3070", 8 * GB - (190 << 20), false),
            card("NVIDIA GeForce RTX 5060 Ti", 8 * GB - (150 << 20), true),
        ];
        let c = choose(&seen, None);
        assert_eq!((c.order, c.why), (vec![1, 0], Why::Display));
    }

    #[test]
    fn equal_cards_keep_windows_order() {
        let seen = [
            card("NVIDIA GeForce RTX 3070", 8 * GB, true),
            card("NVIDIA GeForce RTX 3070", 8 * GB, true),
        ];
        assert_eq!(choose(&seen, None).why, Why::First);
        let seen = [card("A", 8 * GB, false), card("B", 8 * GB, false)];
        assert_eq!(choose(&seen, None).order, vec![0, 1]);
    }

    #[test]
    fn the_variable_takes_an_index() {
        let seen = [
            card("NVIDIA GeForce RTX 5060 Ti", 16 * GB, true),
            card("NVIDIA GeForce RTX 3070", 8 * GB, false),
        ];
        let c = choose(&seen, Some("1"));
        assert_eq!(
            (c.order, c.why, c.unmatched),
            (vec![1, 0], Why::Asked, false)
        );
        assert_eq!(choose(&seen, Some(" 0 ")).order, vec![0, 1]);
    }

    #[test]
    fn the_variable_takes_a_piece_of_the_name() {
        let seen = [
            card("NVIDIA GeForce RTX 3070", 8 * GB, false),
            card("NVIDIA GeForce RTX 5060 Ti", 16 * GB, true),
            card("Intel(R) UHD Graphics 770", 0, false),
        ];
        assert_eq!(choose(&seen, Some("rtx 3070")).order, vec![0, 1, 2]);
        assert_eq!(choose(&seen, Some("intel")).order, vec![2, 1, 0]);
        // A number past the end of the list is a model number, a whole one.
        assert_eq!(choose(&seen, Some("5060")).order, vec![1, 0, 2]);
        assert_eq!(choose(&seen, Some("770")).order, vec![2, 1, 0]);
        // Two match: the better of them first.
        assert_eq!(choose(&seen, Some("NVIDIA")).order, vec![1, 0, 2]);
    }

    #[test]
    fn a_name_that_matches_nothing_falls_back() {
        let seen = [
            card("NVIDIA GeForce RTX 3070", 8 * GB, false),
            card("NVIDIA GeForce RTX 5060 Ti", 16 * GB, true),
        ];
        let c = choose(&seen, Some("radeon"));
        assert_eq!(
            (c.order, c.why, c.unmatched),
            (vec![1, 0], Why::Memory, true)
        );
        // `7` is no adapter's index and no whole number in their names.
        assert!(choose(&seen, Some("7")).unmatched);
        // Empty is unset.
        assert!(!choose(&seen, Some("  ")).unmatched);
    }

    #[test]
    fn no_adapters_chooses_nothing() {
        let c = choose(&[], Some("1"));
        assert!(c.order.is_empty());
        assert!(c.unmatched);
    }
}
