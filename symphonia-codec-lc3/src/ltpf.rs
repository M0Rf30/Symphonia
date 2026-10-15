// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Long term postfilter: a port of the synthesis side of liblc3 `src/ltpf.c` (ETSI TS 103 634
//! clause 3.4.9 "Long Term Postfilter").

use crate::bits::Bits;
use crate::common::{self, DT_2M5, DT_5M, DT_10M};
use crate::tables::{LTPF_CDEN, LTPF_CNUM};

/// Maximum width of the synthesis filter (`MAX_FILTER_WIDTH`, 96 kHz / 4 kHz).
const MAX_FILTER_WIDTH: usize = 24;

/// Bitstream data of the long term postfilter (`struct lc3_ltpf_data`).
#[derive(Copy, Clone, Default)]
pub struct LtpfData {
    pub active: bool,
    pub pitch_index: i32,
}

/// Gets the bitstream data (`lc3_ltpf_get_data`).
pub fn get_data(bits: &mut Bits<'_>) -> LtpfData {
    let active = bits.get_bit() != 0;
    let pitch_index = bits.get_bits(9) as i32;

    LtpfData { active, pitch_index }
}

/// Synthesis filter template (`synthesize_template`).
///
/// `ring` is the history ring buffer of filtered samples (`xh`, of size `nh`). The `n` current
/// samples are in the ring at `xo`, and are filtered in place. `lag` is the lag parameter in the
/// ring buffer. `x0` holds the `W - 1` previous input samples, `c` the coefficients `den` then
/// `num`, and `fade` is the fading mode of the filter (-1: out, 1: in, 0: none).
fn synthesize_template<const W: usize>(
    ring: &mut [f32],
    lag: usize,
    x0: &[f32],
    mut xo: usize,
    n: usize,
    c: &[f32],
    fade: i32,
) {
    let nh = ring.len();

    let mut g = if fade <= 0 { 1.0f32 } else { 0.0 };
    let g_incr = (i32::from(fade > 0) - i32::from(fade < 0)) as f32 / n as f32;
    let mut u = [0.0f32; MAX_FILTER_WIDTH];

    // Load previous samples.
    let lag = lag + (W >> 1);

    let mut y = if xo < lag { (xo + nh).saturating_sub(lag) } else { xo - lag };
    let y_end = nh - 1;

    for j in 0..W - 1 {
        u[j] = 0.0;

        let yi = ring[y];
        let xi = x0[j];
        y = if y < y_end { y + 1 } else { 0 };

        for k in 0..=j {
            u[j - k] -= yi * c[k];
        }

        for k in 0..=j {
            u[j - k] += xi * c[W + k];
        }
    }

    u[W - 1] = 0.0;

    // Process by filter length.
    let mut i = 0;
    while i < n {
        for j in 0..W {
            let yi = ring[y];
            let xi = ring[xo];
            y = if y < y_end { y + 1 } else { 0 };

            for k in 0..W {
                u[(j + (W - 1) - k) % W] -= yi * c[k];
            }

            for k in 0..W {
                u[(j + (W - 1) - k) % W] += xi * c[W + k];
            }

            ring[xo] = xi - g * u[j];
            xo += 1;
            u[j] = 0.0;

            g += g_incr;
        }

        i += W;
    }
}

/// Synthesis filter for the width of the filter of each sample rate (`synthesize`).
#[allow(clippy::too_many_arguments)]
fn synthesize_filter(
    w: usize,
    ring: &mut [f32],
    lag: usize,
    x0: &[f32],
    xo: usize,
    n: usize,
    c: &[f32],
    fade: i32,
) {
    match w {
        4 => synthesize_template::<4>(ring, lag, x0, xo, n, c, fade),
        6 => synthesize_template::<6>(ring, lag, x0, xo, n, c, fade),
        8 => synthesize_template::<8>(ring, lag, x0, xo, n, c, fade),
        _ => synthesize_template::<12>(ring, lag, x0, xo, n, c, fade),
    }
}

/// State of the long term postfilter (`lc3_ltpf_synthesis_t`).
#[derive(Clone)]
pub struct LtpfSynthesis {
    active: bool,
    pitch: i32,
    x: [f32; MAX_FILTER_WIDTH],
    c: [f32; 2 * MAX_FILTER_WIDTH],
}

impl LtpfSynthesis {
    pub fn new() -> Self {
        LtpfSynthesis {
            active: false,
            pitch: 0,
            x: [0.0; MAX_FILTER_WIDTH],
            c: [0.0; 2 * MAX_FILTER_WIDTH],
        }
    }

    /// Synthesis of the postfilter (`lc3_ltpf_synthesize`).
    ///
    /// `ring` is the history ring buffer of size `ns + nh`, and the current frame is in it at
    /// `xo`, and filtered in place. `data` is `None` when the frame has no pitch, or is lost.
    pub fn synthesize(
        &mut self,
        dt: usize,
        sr: usize,
        nbytes: usize,
        data: Option<&LtpfData>,
        ring: &mut [f32],
        xo: usize,
    ) {
        // Filter parameters.
        let p_idx = data.map_or(0, |d| d.pitch_index);
        let mut pitch = if p_idx >= 440 {
            (p_idx - 283) << 2
        }
        else if p_idx >= 380 {
            (((p_idx >> 1) - 63) << 2) + ((p_idx & 1) << 1)
        }
        else {
            (((p_idx >> 2) + 32) << 2) + (p_idx & 3)
        };

        pitch = (pitch * common::ns(DT_10M, sr) as i32 + 64) / 128;

        let mut nbits = (nbytes as i32 * 8 * (1 + DT_10M as i32)) / (1 + dt as i32);
        if dt == DT_2M5 {
            nbits = (6 * nbits + 5) / 10;
        }
        if dt == DT_5M {
            nbits -= 160;
        }

        let g_idx = (nbits / 80).max(3 + sr as i32) - (3 + sr as i32);
        let active = data.is_some_and(|d| d.active) && g_idx < 4;

        let w = 4.max(common::ns_4m(sr) >> 4);
        let mut c = [0.0f32; 2 * MAX_FILTER_WIDTH];

        for i in 0..w {
            let g = if active { 0.4f32 - 0.05f32 * g_idx as f32 } else { 0.0 };
            c[i] = g * LTPF_CDEN[sr][(pitch & 3) as usize][(w - 1) - i];
            c[w + i] = 0.85f32 * g * LTPF_CNUM[sr][g_idx.min(3) as usize][(w - 1) - i];
        }

        // Transition handling.
        let ns = common::ns(dt, sr);
        let nt = ns / (1 + dt);
        let mut x0 = [[0.0f32; MAX_FILTER_WIDTH]; 2];

        x0[0][..w - 1].copy_from_slice(&self.x[..w - 1]);
        self.x[..w - 1].copy_from_slice(&ring[xo + ns - (w - 1)..xo + ns]);

        if active {
            x0[1][..w - 1].copy_from_slice(&ring[xo + nt - (w - 1)..xo + nt]);
        }

        let prev_c = self.c;
        let pitch_lag = (pitch / 4) as usize;
        let prev_lag = (self.pitch / 4) as usize;

        if !self.active && active {
            synthesize_filter(w, ring, pitch_lag, &x0[0], xo, nt, &c, 1);
        }
        else if self.active && !active {
            synthesize_filter(w, ring, prev_lag, &x0[0], xo, nt, &prev_c, -1);
        }
        else if self.active && active && self.pitch == pitch {
            synthesize_filter(w, ring, pitch_lag, &x0[0], xo, nt, &c, 0);
        }
        else if self.active && active {
            synthesize_filter(w, ring, prev_lag, &x0[0], xo, nt, &prev_c, -1);

            // The previous samples are read from the history, and not from the saved input.
            let nh = ring.len();
            let start = if xo == 0 { nh - (w - 1) } else { xo - (w - 1) };
            let mut xp = [0.0f32; MAX_FILTER_WIDTH];
            xp[..w - 1].copy_from_slice(&ring[start..start + (w - 1)]);

            synthesize_filter(w, ring, pitch_lag, &xp, xo, nt, &c, 1);
        }

        // Remainder.
        if active && ns > nt {
            synthesize_filter(w, ring, pitch_lag, &x0[1], xo + nt, ns - nt, &c, 0);
        }

        // Update state.
        self.active = active;
        self.pitch = pitch;
        self.c[..2 * w].copy_from_slice(&c[..2 * w]);
    }
}
