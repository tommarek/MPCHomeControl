//! The multi-rate planning grid: 15-minute blocks near-term, 1-hour blocks further out.
//!
//! The live 36 h horizon at a uniform 15-minute resolution (144 blocks) makes an LP too large for
//! HiGHS to solve within the one-minute tick in winter (2.8 M nonzeros; see
//! `memory/mpchc-36h-lp-unsolvable-in-winter.md`). Coarsening the *far* horizon to hourly blocks
//! cuts the block count ~4× (to `fine_hours*4 + (horizon_hours - fine_hours)`, e.g. 72 for the
//! default 12 h fine / 36 h horizon) while the near-term decisions — the ones actually actuated —
//! stay on the full 15-minute lattice.
//!
//! [`BlockGrid`] is the one source of truth for "how many blocks, how long is each, which fine
//! (15-minute) steps does it cover" — every per-block quantity (prices, PV, load, weather, heat/cool
//! decisions, SoC, timeline) is indexed by a `BlockGrid` block, and every fine-lattice quantity (the
//! thermal condensation, the physics `simulate`) is indexed by its fine step. [`Self::fine_range`],
//! [`Self::mean`]/[`Self::all`]/[`Self::any`]/[`Self::sample_end`] convert between the two.
//!
//! Hourly blocks are **hour-aligned**: VT/NT distribution pricing, hourly day-ahead prices and
//! hourly weather are all keyed to the calendar hour, so an hourly block that straddled a boundary
//! would blend two different hours' values under one price/weather sample. [`Self::multi_rate`]
//! rounds the fine section up to the first calendar-hour boundary at or after `start + fine_hours`
//! and drops a trailing partial hour, so the *effective* horizon is only ever 35–36 h for the live
//! default (36 h configured, 12 h fine) — see its doc for the exact construction.

use std::ops::Range;

use chrono::{DateTime, Duration, Utc};

/// One block: `fine_steps` consecutive fine (15-minute) steps starting at fine index `fine_offset`.
/// `fine_steps` is 1 for a fine block, `3600 / fine_seconds` (4 today) for an hourly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    fine_offset: usize,
    fine_steps: usize,
}

/// The planning grid: `start`, the fine (15-minute) step length, and an ordered list of blocks —
/// each a contiguous run of one or more fine steps. See the module doc.
#[derive(Debug, Clone)]
pub struct BlockGrid {
    pub start: DateTime<Utc>,
    /// The fine-lattice step length (seconds) — the resolution the thermal condensation and the
    /// physics `simulate` run at, and the block length of every fine (near-term) block. 900 (15
    /// min) live; a test may use another value.
    pub fine_seconds: f64,
    blocks: Vec<Block>,
}

impl BlockGrid {
    /// The multi-rate grid: 15-minute (fine) blocks for the first `fine_hours`, hour-aligned, then
    /// 1-hour blocks to the end of `horizon_hours`; a trailing partial hour is dropped.
    ///
    /// `fine_hours >= horizon_hours` degenerates to [`Self::uniform`] at `fine_seconds` over the
    /// whole horizon — no hour-boundary rounding, so this is exactly today's uniform-grid behaviour
    /// (used by tests and `what_if`), not a one-block-short special case.
    ///
    /// Otherwise: the fine section runs from `start` to `fine_end`, the first instant at or after
    /// `start + fine_hours` that lands on a calendar-hour boundary (0 extra fine blocks if
    /// `start + fine_hours` is already hour-aligned, up to `3600/fine_seconds - 1` extra otherwise —
    /// 48–51 fine blocks for the live default). From `fine_end`, whole hours are added while they
    /// still fit before `start + horizon_hours`; a final partial hour (< 1 h) is dropped, so the
    /// grid's total span (see [`Self::n_fine`]) can be up to just under 1 h short of
    /// `horizon_hours`.
    ///
    /// `start` must be aligned to `fine_seconds` (e.g. a quarter-hour for the live 900 s grid) —
    /// debug-asserted; every live/test caller constructs `start` that way already.
    pub fn multi_rate(
        start: DateTime<Utc>,
        horizon_hours: usize,
        fine_hours: usize,
        fine_seconds: f64,
    ) -> Self {
        debug_assert!(horizon_hours >= 1, "horizon_hours must be at least 1");
        debug_assert!(fine_hours >= 1, "fine_hours must be at least 1");
        debug_assert!(fine_seconds > 0.0, "fine_seconds must be positive");
        debug_assert!(
            (3600.0 / fine_seconds).round() * fine_seconds == 3600.0,
            "fine_seconds ({fine_seconds}) must evenly divide an hour"
        );
        let fine_seconds_i = fine_seconds as i64;
        let start_ts = start.timestamp();
        debug_assert!(
            start_ts.rem_euclid(fine_seconds_i) == 0,
            "BlockGrid::multi_rate: start must be aligned to fine_seconds ({fine_seconds}s)"
        );

        if fine_hours >= horizon_hours {
            let total_seconds = horizon_hours as i64 * 3600;
            debug_assert!(total_seconds % fine_seconds_i == 0);
            let n = (total_seconds / fine_seconds_i) as usize;
            return Self::uniform(start, n, fine_seconds);
        }

        let fine_target_ts = start_ts + fine_hours as i64 * 3600;
        let fine_end_ts = if fine_target_ts % 3600 == 0 {
            fine_target_ts
        } else {
            fine_target_ts - fine_target_ts.rem_euclid(3600) + 3600
        };
        let fine_steps_count = ((fine_end_ts - start_ts) / fine_seconds_i) as usize;

        let horizon_end_ts = start_ts + horizon_hours as i64 * 3600;
        let hourly_hours = ((horizon_end_ts - fine_end_ts).max(0) / 3600) as usize;

        let steps_per_hour = (3600.0 / fine_seconds).round() as usize;
        let mut blocks = Vec::with_capacity(fine_steps_count + hourly_hours);
        let mut offset = 0usize;
        for _ in 0..fine_steps_count {
            blocks.push(Block {
                fine_offset: offset,
                fine_steps: 1,
            });
            offset += 1;
        }
        for _ in 0..hourly_hours {
            blocks.push(Block {
                fine_offset: offset,
                fine_steps: steps_per_hour,
            });
            offset += steps_per_hour;
        }

        Self {
            start,
            fine_seconds,
            blocks,
        }
    }

    /// `n` fine blocks of `step_seconds` each, covering `[start, start + n*step_seconds)` — today's
    /// uniform-grid behaviour, exactly reproduced (every block is one fine step).
    pub fn uniform(start: DateTime<Utc>, n: usize, step_seconds: f64) -> Self {
        let blocks = (0..n)
            .map(|i| Block {
                fine_offset: i,
                fine_steps: 1,
            })
            .collect();
        Self {
            start,
            fine_seconds: step_seconds,
            blocks,
        }
    }

    /// Number of blocks in the grid.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Number of fine (15-minute) steps actually covered by the grid — the length every
    /// fine-lattice input (`u_known`, weather, etc.) must have. 144 for a uniform live horizon;
    /// slightly less than `horizon_hours*3600/fine_seconds` for a multi-rate grid whose trailing
    /// partial hour was dropped.
    pub fn n_fine(&self) -> usize {
        self.blocks
            .last()
            .map(|b| b.fine_offset + b.fine_steps)
            .unwrap_or(0)
    }

    /// Block `i`'s duration in hours.
    pub fn dt_hours(&self, i: usize) -> f64 {
        self.blocks[i].fine_steps as f64 * self.fine_seconds / 3600.0
    }

    /// Every block's duration in hours, in order.
    pub fn dt_hours_vec(&self) -> Vec<f64> {
        (0..self.len()).map(|i| self.dt_hours(i)).collect()
    }

    /// The fine-step indices block `i` covers (`fine[range]` is exactly its span).
    pub fn fine_range(&self, i: usize) -> Range<usize> {
        let b = self.blocks[i];
        b.fine_offset..b.fine_offset + b.fine_steps
    }

    /// The UTC instant block `i` starts at.
    pub fn block_start(&self, i: usize) -> DateTime<Utc> {
        self.start
            + Duration::seconds((self.blocks[i].fine_offset as f64 * self.fine_seconds) as i64)
    }

    /// The UTC instant block `i` ends at (the next block's start, or the grid's end for the last
    /// block).
    pub fn block_end(&self, i: usize) -> DateTime<Utc> {
        let b = self.blocks[i];
        self.start
            + Duration::seconds(((b.fine_offset + b.fine_steps) as f64 * self.fine_seconds) as i64)
    }

    /// Per-block MEAN of a fine-lattice value (e.g. prices, PV, load): the block-average, matching
    /// the block-average convention every LP quantity already uses.
    pub fn mean(&self, fine: &[f64]) -> Vec<f64> {
        debug_assert!(fine.len() >= self.n_fine());
        (0..self.len())
            .map(|i| {
                let r = self.fine_range(i);
                let slice = &fine[r.clone()];
                slice.iter().sum::<f64>() / slice.len() as f64
            })
            .collect()
    }

    /// Per-block AND of a fine-lattice flag (e.g. `export_allowed`): true only when every fine step
    /// the block covers is true — an hourly block may only do what ALL of its quarter-hours may.
    pub fn all(&self, fine: &[bool]) -> Vec<bool> {
        debug_assert!(fine.len() >= self.n_fine());
        (0..self.len())
            .map(|i| fine[self.fine_range(i)].iter().all(|&b| b))
            .collect()
    }

    /// Per-block OR of a fine-lattice flag (e.g. `price_is_placeholder`): true when any fine step
    /// the block covers is true.
    pub fn any(&self, fine: &[bool]) -> Vec<bool> {
        debug_assert!(fine.len() >= self.n_fine());
        (0..self.len())
            .map(|i| fine[self.fine_range(i)].iter().any(|&b| b))
            .collect()
    }

    /// Per-block value at the block's LAST fine step (e.g. the free-response temperature sampled at
    /// each block's end, matching today's per-fine-step sampling on a uniform grid).
    pub fn sample_end(&self, fine: &[f64]) -> Vec<f64> {
        debug_assert!(fine.len() >= self.n_fine());
        (0..self.len())
            .map(|i| fine[self.fine_range(i).end - 1])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn uniform_grid_is_all_fine_blocks() {
        let start = utc("2026-01-15T00:15:00Z");
        let grid = BlockGrid::uniform(start, 144, 900.0);
        assert_eq!(grid.len(), 144);
        assert_eq!(grid.n_fine(), 144);
        for i in 0..144 {
            assert_eq!(grid.dt_hours(i), 0.25);
            assert_eq!(grid.fine_range(i), i..i + 1);
        }
        assert_eq!(grid.block_start(0), start);
        assert_eq!(grid.block_end(143), start + Duration::seconds(144 * 900));
    }

    #[test]
    fn multi_rate_on_the_hour_matches_the_documented_default() {
        let start = utc("2026-01-15T00:00:00Z");
        let grid = BlockGrid::multi_rate(start, 36, 12, 900.0);
        // 12h fine (already hour-aligned) = 48 fine blocks, then 24 hourly blocks to 36h.
        assert_eq!(grid.len(), 72);
        assert_eq!(grid.n_fine(), 48 + 24 * 4);
        for i in 0..48 {
            assert_eq!(grid.dt_hours(i), 0.25, "block {i} should be fine");
        }
        for i in 48..72 {
            assert_eq!(grid.dt_hours(i), 1.0, "block {i} should be hourly");
        }
        assert_eq!(grid.block_start(48), start + Duration::hours(12));
        assert_eq!(grid.block_end(71), start + Duration::hours(36));
    }

    #[test]
    fn multi_rate_off_the_hour_rounds_the_fine_section_up() {
        // Start at :15 past the hour, not hour-aligned — exercises the rounding rule.
        let start = utc("2026-01-15T00:15:00Z");
        let grid = BlockGrid::multi_rate(start, 36, 12, 900.0);
        // start + 12h = 12:15, rounds up to 13:00 -> 51 fine blocks (48 + 3 extra quarter-hours).
        assert_eq!(grid.dt_hours(50), 0.25);
        let fine_blocks = (0..grid.len())
            .filter(|&i| grid.dt_hours(i) == 0.25)
            .count();
        assert_eq!(fine_blocks, 51);
        assert_eq!(grid.block_start(51), utc("2026-01-15T13:00:00Z"));
        // Trailing partial hour dropped: horizon end is 00:15 + 36h = 12:15 the next day; the last
        // whole hour that fits before it starts at 11:15... but hourly blocks start at 13:00 and
        // step by 1h, so the last one that fits ends at or before 12:15 - the 23rd hourly block
        // (13:00 + 23h = 12:00) fits, a 24th (13:00+24h=13:00, past 12:15) does not.
        let hourly_blocks = grid.len() - fine_blocks;
        assert_eq!(hourly_blocks, 23);
        // Effective horizon a little under 36h (35.75h here): n_fine * 900s.
        let effective_hours = grid.n_fine() as f64 * 900.0 / 3600.0;
        assert!((34.0..36.0).contains(&effective_hours), "{effective_hours}");
    }

    #[test]
    fn fine_hours_at_least_horizon_hours_degenerates_to_uniform() {
        let start = utc("2026-01-15T00:15:00Z");
        let multi = BlockGrid::multi_rate(start, 36, 36, 900.0);
        let uniform = BlockGrid::uniform(start, 144, 900.0);
        assert_eq!(multi.len(), uniform.len());
        assert_eq!(multi.dt_hours_vec(), uniform.dt_hours_vec());
        assert_eq!(multi.n_fine(), uniform.n_fine());

        // fine_hours > horizon_hours also degenerates (not just equal).
        let over = BlockGrid::multi_rate(start, 24, 48, 900.0);
        assert_eq!(over.len(), 96);
        assert!(over.dt_hours_vec().iter().all(|&dt| dt == 0.25));
    }

    #[test]
    fn mean_all_any_sample_end_aggregate_fine_to_block() {
        let start = utc("2026-01-15T00:00:00Z");
        let grid = BlockGrid::multi_rate(start, 2, 1, 900.0);
        // 1h fine (4 blocks) + 1 hourly block = 5 blocks, 8 fine steps.
        assert_eq!(grid.len(), 5);
        assert_eq!(grid.n_fine(), 8);

        let fine_vals: Vec<f64> = (0..8).map(|i| i as f64).collect();
        let means = grid.mean(&fine_vals);
        assert_eq!(means.len(), 5);
        assert_eq!(&means[0..4], &[0.0, 1.0, 2.0, 3.0]); // fine blocks pass through
        assert_eq!(means[4], (4.0 + 5.0 + 6.0 + 7.0) / 4.0); // hourly block averages its 4 steps

        let ends = grid.sample_end(&fine_vals);
        assert_eq!(&ends[0..4], &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(ends[4], 7.0); // last fine step of the hourly block

        let all_true = vec![true; 8];
        assert!(grid.all(&all_true).iter().all(|&b| b));
        let mut mixed = vec![true; 8];
        mixed[7] = false; // one quarter-hour of the hourly block is false
        assert!(
            !grid.all(&mixed)[4],
            "all() must be false if any fine step is false"
        );
        assert!(
            grid.any(&mixed)[4],
            "any() must be true since 3 of 4 are still true"
        );
        let all_false = vec![false; 8];
        assert!(!grid.any(&all_false)[4]);
    }
}
