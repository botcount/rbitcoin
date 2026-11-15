//! Historical fee hurdles from windows that looked like now.
//!
//! Each block with a hurdle (its vsize-weighted p10 feerate) is one
//! observation; blocks without one (coinbase-only, or every tx below min
//! relay) are skipped, so a window of N is N blocks that carried
//! transactions. For target N, the pair at position `t` is the median hurdle
//! of the [`analog_lookback`] blocks before `t` and the lowest hurdle of the N
//! blocks from `t` (a tx at that rate would have beaten some block's p10).
//! An estimate keeps pairs whose lookback median is within [`ANALOG_BAND`]
//! of the current lookback median, or the [`ANALOG_MIN_NEIGHBORS`] nearest,
//! and takes the requested quantile of their outcomes. Windows that started
//! inside a spike stop steering calm-market estimates, and the reverse.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

/// Lookback medians within this ratio of now count as "looked like now".
pub const ANALOG_BAND: f64 = 1.25;
/// Fewest neighbors an estimate uses; below it, the nearest by lookback.
pub const ANALOG_MIN_NEIGHBORS: usize = 200;
/// Pairs a target needs before it answers.
pub const ANALOG_READY_PAIRS: usize = 2_000;
/// Quantile for targets past one block. Flow fill and λ keep 0.99.
pub const HISTORICAL_CONFIDENCE_FAR: f64 = 0.95;

/// Lookback length for target N: `clamp(N/4, 3, 144)` hurdle blocks. On a
/// mainnet backtest a 3-block floor tracked the current level closer than 6
/// at N=2–6 (99.1–99.3% coverage at 99% vs 98.6–98.8%, and lower rates).
pub fn analog_lookback(n_blocks: u32) -> usize {
    (n_blocks as usize / 4).clamp(3, 144)
}

fn blocks_in_window(n_blocks: u32) -> NonZeroUsize {
    match NonZeroUsize::new(n_blocks as usize) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// Lowest hurdle in `hurdles[start..start + n]`. `n` is at least one.
fn lowest_hurdle(hurdles: &VecDeque<u64>, start: usize, n: NonZeroUsize) -> u64 {
    let mut low = hurdles[start];
    for h in hurdles.range(start + 1..start + n.get()).copied() {
        low = low.min(h);
    }
    low
}

#[derive(Debug)]
#[doc(hidden)]
pub struct Depth {
    pub n: NonZeroUsize,
    pub lookback: usize,
    /// `(ln lookback median, window hurdle)` by window start, oldest first.
    pub pairs: VecDeque<(f64, u64)>,
}

/// Hurdle sequence plus per-target analog pairs, kept in step as blocks are
/// added or dropped at either end.
///
/// RAM: 16 B per pair per target plus 8 B per hurdle; ~5.5 MiB for 31k
/// mainnet blocks (1 GiB of txstat) at 11 targets. CPU: an end push or pop
/// costs `O(lookback·log + N)` per target; [`Self::rate_sat_kvb`] scans every
/// pair of one target (~0.2 ms at 31k).
#[derive(Debug)]
pub struct AnalogHistory {
    pub hurdles: VecDeque<u64>,
    pub depths: Vec<Depth>,
}

#[doc(hidden)]
pub fn lower_median(values: impl Iterator<Item = u64>) -> u64 {
    let mut v: Vec<u64> = values.collect();
    let mid = (v.len() - 1) / 2;
    *v.select_nth_unstable(mid).1
}

#[doc(hidden)]
pub fn ln_rate(rate: u64) -> f64 {
    (rate.max(1) as f64).ln()
}

impl AnalogHistory {
    pub fn new(targets: &[u32]) -> Self {
        Self {
            hurdles: VecDeque::new(),
            depths: targets
                .iter()
                .map(|&n| Depth {
                    n: blocks_in_window(n),
                    lookback: analog_lookback(n),
                    pairs: VecDeque::new(),
                })
                .collect(),
        }
    }

    /// Pair whose window starts at hurdle index `t`.
    #[doc(hidden)]
    pub fn pair(hurdles: &VecDeque<u64>, d: &Depth, t: usize) -> (f64, u64) {
        let before = lower_median(hurdles.range(t - d.lookback..t).copied());
        let ahead = lowest_hurdle(hurdles, t, d.n);
        (ln_rate(before), ahead)
    }

    /// Append the newest block's hurdle.
    pub fn push_back(&mut self, hurdle: u64) {
        self.hurdles.push_back(hurdle);
        let k = self.hurdles.len();
        for d in &mut self.depths {
            if k >= d.lookback + d.n.get() {
                let pair = Self::pair(&self.hurdles, d, k - d.n.get());
                d.pairs.push_back(pair);
            }
        }
    }

    /// Drop the newest block's hurdle (its windows end with it).
    pub fn pop_back(&mut self) {
        if self.hurdles.pop_back().is_some() {
            for d in &mut self.depths {
                d.pairs.pop_back();
            }
        }
    }

    /// Prepend an older block's hurdle.
    pub fn push_front(&mut self, hurdle: u64) {
        self.hurdles.push_front(hurdle);
        let k = self.hurdles.len();
        for d in &mut self.depths {
            if k >= d.lookback + d.n.get() {
                let pair = Self::pair(&self.hurdles, d, d.lookback);
                d.pairs.push_front(pair);
            }
        }
    }

    /// Drop the oldest block's hurdle (its lookbacks start with it).
    pub fn pop_front(&mut self) {
        if self.hurdles.pop_front().is_some() {
            for d in &mut self.depths {
                d.pairs.pop_front();
            }
        }
    }

    /// Replace the sequence (oldest first).
    pub fn rebuild(&mut self, hurdles: impl IntoIterator<Item = u64>) {
        self.hurdles.clear();
        for d in &mut self.depths {
            d.pairs.clear();
        }
        for hurdle in hurdles {
            self.push_back(hurdle);
        }
    }

    /// Pairs held for target `n_blocks` (0 for a target not tracked).
    pub fn pairs(&self, n_blocks: u32) -> usize {
        self.depths
            .iter()
            .find(|d| d.n == blocks_in_window(n_blocks))
            .map_or(0, |d| d.pairs.len())
    }

    /// Lowest rate (sat/kvB) at or above the window hurdle in a `confidence`
    /// share of the windows that looked like now. None before the target
    /// holds [`ANALOG_READY_PAIRS`] pairs.
    pub fn rate_sat_kvb(&self, n_blocks: u32, confidence: f64) -> Option<u64> {
        let d = self
            .depths
            .iter()
            .find(|d| d.n == blocks_in_window(n_blocks))?;
        if d.pairs.len() < ANALOG_READY_PAIRS || !(0.0..=1.0).contains(&confidence) {
            return None;
        }
        let k = self.hurdles.len();
        let now = ln_rate(lower_median(self.hurdles.range(k - d.lookback..).copied()));
        let band = ANALOG_BAND.ln();
        let mut outcomes: Vec<u64> = d
            .pairs
            .iter()
            .filter(|(before, _)| (before - now).abs() <= band)
            .map(|&(_, ahead)| ahead)
            .collect();
        // Out-of-band distances sit past the band, so a full in-band set is
        // already the nearest floor. Borrow only while some neighbors are missing.
        let missing = ANALOG_MIN_NEIGHBORS.saturating_sub(outcomes.len());
        if missing > 0 {
            let mut nearest: Vec<(f64, u64)> = d
                .pairs
                .iter()
                .map(|&(before, ahead)| ((before - now).abs(), ahead))
                .collect();
            nearest.select_nth_unstable_by(ANALOG_MIN_NEIGHBORS.saturating_sub(1), |a, b| {
                a.0.total_cmp(&b.0)
            });
            outcomes = nearest[..ANALOG_MIN_NEIGHBORS]
                .iter()
                .map(|&(_, ahead)| ahead)
                .collect();
        }
        Some(log_quantile(&mut outcomes, confidence))
    }
}

/// Log-rate interpolation between the two order statistics around `confidence`.
/// A rank that lands on one hurdle, including a repeated one, stays there.
#[doc(hidden)]
pub fn log_quantile(outcomes: &mut [u64], confidence: f64) -> u64 {
    let n = outcomes.len();
    outcomes.sort_unstable();
    let rank = (confidence * n as f64).clamp(1.0, n as f64);
    let lo = rank.floor() as usize - 1;
    let hi = rank.ceil() as usize - 1;
    if lo == hi || outcomes[lo] == outcomes[hi] {
        return outcomes[lo];
    }
    let frac = rank - rank.floor();
    let blended = (1.0 - frac) * (outcomes[lo] as f64).ln() + frac * (outcomes[hi] as f64).ln();
    blended.exp().round() as u64
}
