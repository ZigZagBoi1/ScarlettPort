//! The sums behind `SCARLETT_GPU_PROFILE`'s report: no host objects, so it
//! is tested on its own (`rustc --test gpuprof/agg.rs`).

use std::collections::HashMap;
use std::fmt::Write as _;

/// One timed list as read back: its timestamps (first where it began, one
/// at each pipeline change, last where it ended) and, per span between two
/// stamps, the pipeline key and the draws and dispatches in it.
pub(crate) struct ListStamps<'a> {
    pub(crate) stamps: &'a [u64],
    pub(crate) keys: &'a [u32],
    pub(crate) draws: &'a [u32],
}

/// Key of the span before a list's first pipeline (copies, barriers).
pub(crate) const PRELUDE: u32 = u32::MAX;
/// Key of what a list recorded after its stamps ran out.
pub(crate) const OVERFLOW: u32 = u32::MAX - 1;
/// Key of a whole list that bound no pipeline (copies, clears, barriers
/// only): kept apart from [`PRELUDE`], which is then only what lists that
/// do draw record before their first pipeline.
pub(crate) const NOPIPE: u32 = u32::MAX - 2;

/// Gaps between title lists on the card's clock, by length: under
/// [`GAP_SHORT_US`] microseconds the next list followed at once
/// (the switch from one list to the next), up to [`GAP_LONG_US`] a short
/// wait, above it the queue had none of the title's lists to run.
pub(crate) const GAP_SHORT_US: u64 = 50;
pub(crate) const GAP_LONG_US: u64 = 1000;

/// What the card's clock says of the title's lists as a whole: the time
/// inside them, and the time between them by gap length.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) struct Busy {
    /// Ticks inside some title list (their union).
    pub(crate) busy: u64,
    /// Ticks from the first list's start to the last one's end.
    pub(crate) extent: u64,
    /// Gaps (count, ticks): short, middle, long.
    pub(crate) gaps: [(u64, u64); 3],
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sum {
    pub(crate) ticks: u64,
    pub(crate) draws: u64,
    pub(crate) groups: u64,
}

#[derive(Default)]
pub(crate) struct Agg {
    pub(crate) by_key: HashMap<u32, Sum>,
    pub(crate) list_ticks: u64,
    pub(crate) lists: u64,
    /// Lists dropped: never ran, or stamps out of order.
    pub(crate) unread: u64,
    pub(crate) frames: u64,
    /// Every counted list's first and last stamp, for [`Agg::busy`].
    pub(crate) spans: Vec<(u64, u64)>,
}

/// Ticks between two stamps; 0 when the second is not after the first or
/// the gap is beyond any frame's (`cap` ticks).
pub(crate) fn span(begin: u64, end: u64, cap: u64) -> u64 {
    if begin == 0 || end <= begin { 0 } else { (end - begin).min(cap) }
}

impl Agg {
    /// Adds one list. `cap`: the most ticks one span may count (a second).
    pub(crate) fn add_list(&mut self, l: ListStamps, cap: u64) {
        let n = l.stamps.len();
        if n < 2 || l.keys.len() != n - 1 || l.draws.len() != n - 1 || l.stamps[0] == 0 || l.stamps[n - 1] == 0 {
            self.unread += 1;
            return;
        }
        self.lists += 1;
        self.list_ticks += span(l.stamps[0], l.stamps[n - 1], cap);
        if l.stamps[n - 1] > l.stamps[0] {
            self.spans.push((l.stamps[0], l.stamps[n - 1]));
        }
        let no_pipeline = n == 2 && l.keys[0] == PRELUDE;
        for i in 0..n - 1 {
            let key = if no_pipeline { NOPIPE } else { l.keys[i] };
            let s = self.by_key.entry(key).or_default();
            s.ticks += span(l.stamps[i], l.stamps[i + 1], cap);
            s.draws += u64::from(l.draws[i]);
            s.groups += 1;
        }
    }

    pub(crate) fn frame(&mut self) {
        self.frames += 1;
    }

    /// The title lists on the card's clock: time inside them and the gaps
    /// between them (`frequency`: ticks a second, for the gap classes).
    /// Lists overlapping or touching count once.
    pub(crate) fn busy(&self, frequency: u64) -> Busy {
        let mut v = self.spans.clone();
        v.sort_unstable();
        let mut out = Busy::default();
        let Some(&(first, mut end)) = v.first() else {
            return out;
        };
        let mut begin = first;
        let us = |ticks: u64| ticks.saturating_mul(1_000_000) / frequency.max(1);
        for &(a, b) in &v[1..] {
            if a <= end {
                end = end.max(b);
                continue;
            }
            out.busy += end - begin;
            let gap = a - end;
            let class = match us(gap) {
                g if g < GAP_SHORT_US => 0,
                g if g < GAP_LONG_US => 1,
                _ => 2,
            };
            out.gaps[class].0 += 1;
            out.gaps[class].1 += gap;
            (begin, end) = (a, b);
        }
        out.busy += end - begin;
        out.extent = end - first;
        out
    }

    /// The `n` keys with most time, most first: (key, sum).
    pub(crate) fn top(&self, n: usize) -> Vec<(u32, Sum)> {
        let mut v: Vec<(u32, Sum)> = self.by_key.iter().map(|(&k, &s)| (k, s)).collect();
        v.sort_by(|a, b| b.1.ticks.cmp(&a.1.ticks).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// The report: per frame, ms and draws of the top `n` pipelines.
    pub(crate) fn report(&self, n: usize, frequency: u64, label: &dyn Fn(u32) -> String) -> String {
        let frames = self.frames.max(1) as f64;
        let ms = |t: u64| t as f64 * 1e3 / frequency.max(1) as f64 / frames;
        let all: u64 = self.by_key.values().map(|s| s.ticks).sum();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "GPU profile over {} frames: title lists {:.2} ms/frame ({:.1} lists/frame, {} unread), pipeline spans {:.2} ms/frame, {} pipelines",
            self.frames,
            ms(self.list_ticks),
            self.lists as f64 / frames,
            self.unread,
            ms(all),
            self.by_key.len()
        );
        // The card's own clock over all title lists: inside them, and
        // between them (the card idle, waiting for the next submission, or
        // running the layer's own lists: state preludes, card sync and
        // check lists, the present).
        let b = self.busy(frequency);
        let pct = |t: u64| if b.extent == 0 { 0.0 } else { t as f64 * 100.0 / b.extent as f64 };
        let between = b.extent.saturating_sub(b.busy);
        let _ = writeln!(
            out,
            "  GPU busy: in title lists {:.2} ms/frame ({:.1}% of the card's clock), between them {:.2} ms/frame ({:.1}%):              {:.1} gaps/frame under {GAP_SHORT_US} us = {:.2} ms (list to list), {:.1} of {GAP_SHORT_US}-{GAP_LONG_US} us = {:.2} ms,              {:.1} over {GAP_LONG_US} us = {:.2} ms (no title list to run: waiting for a submission, or the layer's own lists)",
            ms(b.busy),
            pct(b.busy),
            ms(between),
            pct(between),
            b.gaps[0].0 as f64 / frames,
            ms(b.gaps[0].1),
            b.gaps[1].0 as f64 / frames,
            ms(b.gaps[1].1),
            b.gaps[2].0 as f64 / frames,
            ms(b.gaps[2].1),
        );
        for (i, (k, s)) in self.top(n).into_iter().enumerate() {
            let name = match k {
                PRELUDE => "(list start, before the first pipeline of a list that draws)".to_owned(),
                NOPIPE => "(lists that bind no pipeline: copies, clears, barriers)".to_owned(),
                OVERFLOW => "(after the list's stamps ran out)".to_owned(),
                k => label(k),
            };
            let _ = writeln!(
                out,
                "  #{:<2} {:>7.3} ms/frame {:>5.1}%  {:>7.1} draws/frame {:>6.1} groups/frame  {}",
                i + 1,
                ms(s.ticks),
                if all == 0 { 0.0 } else { s.ticks as f64 * 100.0 / all as f64 },
                s.draws as f64 / frames,
                s.groups as f64 / frames,
                name
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(stamps: &[u64], keys: &[u32], draws: &[u32]) -> ListStamps<'static> {
        ListStamps {
            stamps: Box::leak(stamps.to_vec().into_boxed_slice()),
            keys: Box::leak(keys.to_vec().into_boxed_slice()),
            draws: Box::leak(draws.to_vec().into_boxed_slice()),
        }
    }

    #[test]
    fn spans_go_to_their_pipeline_and_lists_to_the_total() {
        let mut a = Agg::default();
        a.add_list(list(&[100, 110, 150, 160], &[PRELUDE, 7, 9], &[0, 3, 1]), 1 << 40);
        a.add_list(list(&[200, 240, 241], &[7, 9], &[2, 5]), 1 << 40);
        a.frame();
        assert_eq!(a.lists, 2);
        assert_eq!(a.list_ticks, 60 + 41);
        assert_eq!(a.by_key[&7], Sum { ticks: 40 + 40, draws: 5, groups: 2 });
        assert_eq!(a.by_key[&9], Sum { ticks: 10 + 1, draws: 6, groups: 2 });
        assert_eq!(a.by_key[&PRELUDE], Sum { ticks: 10, draws: 0, groups: 1 });
        let top = a.top(2);
        assert_eq!(top.iter().map(|t| t.0).collect::<Vec<_>>(), vec![7, 9]);
    }

    #[test]
    fn lists_that_never_ran_or_are_malformed_are_not_counted() {
        let mut a = Agg::default();
        a.add_list(list(&[0, 0, 0], &[1, 2], &[1, 1]), 1000);
        a.add_list(list(&[5, 9], &[1, 2], &[1, 1]), 1000);
        a.add_list(list(&[5], &[], &[]), 1000);
        assert_eq!((a.lists, a.unread), (0, 3));
        assert!(a.by_key.is_empty());
    }

    #[test]
    fn backwards_and_huge_spans_are_bounded() {
        assert_eq!(span(10, 5, 100), 0);
        assert_eq!(span(0, 5, 100), 0);
        assert_eq!(span(1, u64::MAX, 100), 100);
        let mut a = Agg::default();
        a.add_list(list(&[10, 8, 20], &[1, 2], &[1, 1]), 1000);
        assert_eq!(a.by_key[&1].ticks, 0);
        assert_eq!(a.by_key[&2].ticks, 12);
    }

    #[test]
    fn report_is_per_frame_and_names_the_top() {
        let mut a = Agg::default();
        // 1 MHz clock: 1 tick = 1 us.
        for _ in 0..4 {
            a.add_list(list(&[1_000, 9_000, 11_000], &[3, 4], &[10, 2]), 1 << 40);
            a.frame();
        }
        let r = a.report(1, 1_000_000, &|k| format!("cs {k:016x}"));
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("title lists 10.00 ms/frame (1.0 lists/frame"), "{r}");
        assert!(lines[1].contains("GPU busy: in title lists 2.50 ms/frame (100.0%"), "{r}");
        assert!(lines[2].contains("8.000 ms/frame"), "{r}");
        assert!(lines[2].contains("10.0 draws/frame"), "{r}");
        assert!(lines[2].contains("cs 0000000000000003"), "{r}");
        assert!(lines[2].contains("80.0%"), "{r}");
    }

    #[test]
    fn busy_is_the_union_and_gaps_go_by_length() {
        let mut a = Agg::default();
        // 1 MHz clock. Lists: 1000-2000, 1500-2500 (overlap), 2510-3000
        // (10 us gap), 3500-4000 (500 us gap), 9000-9100 (5 ms gap).
        for (s, e) in [(3500, 4000), (1000, 2000), (9000, 9100), (1500, 2500), (2510, 3000)] {
            a.add_list(list(&[s, e], &[5], &[1]), 1 << 40);
        }
        let b = a.busy(1_000_000);
        assert_eq!(b.busy, 1500 + 490 + 500 + 100);
        assert_eq!(b.extent, 8100);
        assert_eq!(b.gaps, [(1, 10), (1, 500), (1, 5000)]);
        assert_eq!(b.busy + b.gaps.iter().map(|g| g.1).sum::<u64>(), b.extent);
        assert_eq!(Agg::default().busy(1_000_000), Busy::default());
    }

    #[test]
    fn a_list_without_a_pipeline_is_not_list_start() {
        let mut a = Agg::default();
        a.add_list(list(&[100, 130], &[PRELUDE], &[0]), 1 << 40);
        a.add_list(list(&[200, 210, 250], &[PRELUDE, 7], &[0, 1]), 1 << 40);
        assert_eq!(a.by_key[&NOPIPE].ticks, 30);
        assert_eq!(a.by_key[&PRELUDE].ticks, 10);
    }
}
