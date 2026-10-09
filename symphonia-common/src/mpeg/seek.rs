// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bisection seeking for MPEG streams that carry a timestamp in-band, but no index.

/// Search the byte range `lo..hi` of a stream for the last position at which a PES packet with a
/// time of at most `target` begins.
///
/// `probe(pos)` must find the first PES packet starting at or after the byte position `pos`, and
/// return its position and time (in any unit and offset consistent with `target`), or `None` if
/// there is none (before the end of the stream). The time of the PES packet at `lo` must be at
/// most `target`.
///
/// The search stops when the range is smaller than `min_gap` bytes, and the position to start
/// scanning from is returned. Probe errors abort the search.
pub fn bisect<E>(
    lo: u64,
    hi: u64,
    target: i64,
    min_gap: u64,
    mut probe: impl FnMut(u64) -> Result<Option<(u64, i64)>, E>,
) -> Result<u64, E> {
    let (mut lo, mut hi) = (lo, hi);

    while hi.saturating_sub(lo) > min_gap {
        let mid = lo + (hi - lo) / 2;

        match probe(mid)? {
            Some((pos, time)) if pos < hi => {
                if time <= target {
                    lo = pos.max(lo + 1);
                }
                else {
                    hi = pos;
                }
            }
            _ => hi = mid,
        }
    }

    Ok(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bisect() {
        // PES packets every 1000 bytes (starting at 0) with a time of 7 ticks per 1000 bytes.
        let len = 1_000_000u64;
        let probe = |pos: u64| -> Result<Option<(u64, i64)>, ()> {
            let p = pos.div_ceil(1000) * 1000;
            Ok(if p < len { Some((p, (p / 1000 * 7) as i64)) } else { None })
        };

        for target in [0i64, 1, 6, 7, 8, 3500, 6992, 6993, 100_000] {
            let lo = bisect(0, len, target, 4096, probe).unwrap();
            // The start position has a time at most the target, and it is close.
            let time = (lo / 1000 * 7) as i64;
            assert!(time <= target.min(6993), "target={target} lo={lo}");
            assert!(lo % 1000 == 0);
            // The range to scan forward is small.
            let want = (target.min(6993) / 7 * 1000) as u64;
            assert!(want - lo <= 5000, "target={target} lo={lo} want={want}");
        }
    }
}
