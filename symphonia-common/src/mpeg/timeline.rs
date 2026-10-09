// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Synthesis of a continuous, sample-accurate, packet timeline from the (coarse, 90 kHz) PTS of
//! PES packets.

use super::pes::{PTS_CLOCK_HZ, unwrap_pts};

/// Convert a time in 90 kHz clock ticks to timebase ticks, rounding to the nearest.
pub fn clock_to_ticks(clock: i64, rate: u32) -> i64 {
    let num = i128::from(clock) * i128::from(rate);
    let den = i128::from(PTS_CLOCK_HZ);
    let half = den / 2;

    let ticks = if num >= 0 { (num + half) / den } else { (num - half) / den };
    ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Convert a time in timebase ticks to 90 kHz clock ticks, rounding down.
pub fn ticks_to_clock(ticks: i64, rate: u32) -> i64 {
    let clock = (i128::from(ticks) * i128::from(PTS_CLOCK_HZ)).div_euclid(i128::from(rate.max(1)));
    clock.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// A `Timeline` assigns timestamps, in timebase ticks (typically samples) from the start of the
/// stream, to the frames of an elementary stream.
///
/// The frames of an elementary stream are contiguous, so the timestamp of a frame is the timestamp
/// of the previous frame plus its duration. This is more precise than the 90 kHz PTS, which also
/// is only present for some frames. A PTS is only used to *anchor* the timeline at the start of
/// the stream, after a seek, and when it indicates a discontinuity (a gap or an overlap larger than
/// can be explained by the PTS precision).
#[derive(Clone, Debug)]
pub struct Timeline {
    /// The timebase ticks per second.
    rate: u32,
    /// The PTS (33-bit, raw) of timestamp 0.
    base: Option<u64>,
    /// The timestamp of the next frame.
    next: i64,
    /// The time, in clock ticks since `base`, last seen. Used to unwrap the PTS.
    last_clock: i64,
    /// If true, the next PTS anchors the timeline unconditionally.
    force_anchor: bool,
    /// If non-zero, the duration of a frame that all frames of the stream have. A timestamp that
    /// is derived from a PTS is rounded to the nearest multiple of it.
    grid: u64,
}

impl Timeline {
    /// Create a timeline for a timebase of `1 / rate`.
    pub fn new(rate: u32) -> Self {
        Timeline { rate, base: None, next: 0, last_clock: 0, force_anchor: true, grid: 0 }
    }

    /// Set the duration of the frames of the stream, if all frames have the same duration. The
    /// frames of the stream are then aligned to a grid of this duration, so that the timestamps
    /// are the same whether the stream is read from the start or after a seek (where the timeline
    /// is anchored on a PTS, which has a precision of 1/90000 s, or worse).
    pub fn set_grid(&mut self, grid: u64) {
        self.grid = grid;
    }

    /// Convert a time in clock ticks since the PTS of timestamp 0 to a timestamp, aligned to the
    /// grid of the frames.
    pub fn ticks_for_clock(&self, clock: i64) -> i64 {
        let ticks = clock_to_ticks(clock, self.rate);

        if self.grid == 0 {
            return ticks;
        }

        let grid = self.grid as i64;
        (ticks + grid / 2).div_euclid(grid) * grid
    }

    /// The PTS of timestamp 0, if known.
    pub fn base(&self) -> Option<u64> {
        self.base
    }

    /// Set the PTS of timestamp 0.
    pub fn set_base(&mut self, base: u64) {
        self.base = Some(base);
    }

    /// The timestamp of the next frame.
    pub fn next_ts(&self) -> i64 {
        self.next
    }

    /// Prepare for a discontinuity of the stream (i.e., a seek). The next PTS will anchor the
    /// timeline. `hint_clock` is an approximation of the position, in clock ticks since the PTS
    /// of timestamp 0, used to unwrap the PTS.
    pub fn discontinuity(&mut self, hint_clock: i64) {
        self.last_clock = hint_clock;
        self.force_anchor = true;
    }

    /// Convert a raw PTS to clock ticks since the PTS of timestamp 0 near `hint`.
    pub fn clock_of(&self, pts: u64, hint: i64) -> i64 {
        unwrap_pts(pts, self.base.unwrap_or(pts), hint)
    }

    /// Assign a timestamp to a frame of duration `dur` that begins at the (raw) PTS `pts`, if
    /// known. Returns the timestamp.
    pub fn stamp(&mut self, pts: Option<u64>, dur: u64) -> i64 {
        if let Some(pts) = pts {
            let base = *self.base.get_or_insert(pts);
            let clock = unwrap_pts(pts, base, self.last_clock);
            let target = self.ticks_for_clock(clock);

            // The precision of the PTS is 1 / 90000 s, but allow for the mux to have rounded
            // to a coarser resolution, such as milliseconds.
            let tolerance = (u64::from(self.rate) / 100).max(dur / 2);

            if self.force_anchor || target.abs_diff(self.next) > tolerance {
                self.next = target;
                self.force_anchor = false;
            }

            self.last_clock = clock;
        }

        let ts = self.next;
        self.next = self.next.saturating_add(dur as i64);
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpeg::pes::PTS_MODULUS;

    #[test]
    fn verify_conversions() {
        assert_eq!(clock_to_ticks(90_000, 44_100), 44_100);
        assert_eq!(clock_to_ticks(1, 44_100), 0);
        assert_eq!(clock_to_ticks(2, 44_100), 1);
        assert_eq!(ticks_to_clock(44_100, 44_100), 90_000);
    }

    #[test]
    fn verify_timeline_is_continuous_and_anchors_on_gaps() {
        let mut tl = Timeline::new(48_000);
        let dur = 1152;

        // The first frame anchors the timeline at 0.
        assert_eq!(tl.stamp(Some(1_000_000), dur), 0);
        // Frames without a PTS are contiguous.
        assert_eq!(tl.stamp(None, dur), 1152);
        // A PTS that agrees (to the precision of the clock) does not change the timeline.
        let pts = 1_000_000 + 2 * 1152 * 90_000 / 48_000;
        assert_eq!(tl.stamp(Some(pts), dur), 2304);
        // A gap of 1 s is honoured.
        let pts = 1_000_000 + 90_000 + 3 * 1152 * 90_000 / 48_000;
        assert_eq!(tl.stamp(Some(pts), dur), 48_000 + 3456);
    }

    #[test]
    fn verify_timeline_wraps() {
        let mut tl = Timeline::new(90_000);
        let start = PTS_MODULUS - 1_000;

        assert_eq!(tl.stamp(Some(start), 900), 0);
        assert_eq!(tl.stamp(Some(start + 900), 900), 900);
        // The PTS wraps.
        assert_eq!(tl.stamp(Some((start + 1800) % PTS_MODULUS), 900), 1800);
        assert_eq!(tl.stamp(Some((start + 2700) % PTS_MODULUS), 900), 2700);
    }
}
