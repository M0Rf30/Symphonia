// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! E-AC-3 transient pre-noise processing (ETSI TS 102 366 clause E.3.7).
//!
//! The encoder transmits, per full bandwidth channel and frame, the location of a transient and a
//! time scaling length. The decoder overwrites the audio before the transient (the pre-noise)
//! with earlier, noise-free audio, cross-fading at both ends.
//!
//! Output is emitted frame by frame, so only the part of a correction that lies in the frame
//! being decoded can be applied: corrections that reach into a frame already emitted are
//! clipped, and corrections that extend into the next frame are carried over and completed when
//! that frame has been decoded.

use crate::core::MAX_BLOCKS;
use crate::imdct::BLOCK_SIZE;

/// Time scaling synthesis system parameters (clause E.3.7.2).
const TC1: i64 = 256;
const TC2: i64 = 128;

const FRAME_SIZE: usize = BLOCK_SIZE * MAX_BLOCKS;

/// The maximum number of full bandwidth channels.
pub const MAX_FBW: usize = 5;

/// The maximum number of corrections of a channel that are open at once: those carried over from
/// earlier frames and the one transmitted in the current frame.
const MAX_OPEN: usize = 3;

/// A correction: the transient location relative to the first sample of a frame, in samples, and
/// the time scaling length.
#[derive(Copy, Clone)]
struct Correction {
    loc: i64,
    len: i64,
}

/// Transient pre-noise data of one frame: the transient location in units of four samples
/// (`transprocloc`) and the time scaling length (`transproclen`), per full bandwidth channel.
pub type FrameData = [Option<(u16, u8)>; MAX_FBW];

pub struct PreNoise {
    /// The unmodified decoded audio of the previous frame.
    hist: Box<[[f32; FRAME_SIZE]; MAX_FBW]>,
    hist_len: usize,
    /// Corrections carried over to the next frame.
    open: [[Option<Correction>; MAX_OPEN]; MAX_FBW],
}

impl PreNoise {
    pub fn new() -> Self {
        PreNoise {
            hist: Box::new([[0.0; FRAME_SIZE]; MAX_FBW]),
            hist_len: 0,
            open: [[None; MAX_OPEN]; MAX_FBW],
        }
    }

    /// Discards all history.
    pub fn clear(&mut self) {
        self.hist_len = 0;
        self.open = [[None; MAX_OPEN]; MAX_FBW];
    }

    /// Processes the decoded frame `pcm` (one row per coded channel, `len` samples each).
    pub fn process_frame(
        &mut self,
        pcm: &mut [[f32; FRAME_SIZE]],
        fbw: usize,
        len: usize,
        data: &FrameData,
    ) {
        let fbw = fbw.min(MAX_FBW);

        for ch in 0..fbw {
            let mut orig = [0.0f32; FRAME_SIZE];
            orig[..len].copy_from_slice(&pcm[ch][..len]);

            let mut todo = [None; MAX_OPEN];
            let mut count = 0;
            for c in self.open[ch].iter().flatten() {
                todo[count] = Some(*c);
                count += 1;
            }
            if let Some((loc, translen)) = data[ch] {
                if count < MAX_OPEN {
                    todo[count] =
                        Some(Correction { loc: i64::from(loc) * 4, len: i64::from(translen) });
                }
            }

            self.open[ch] = [None; MAX_OPEN];
            let mut next = 0;

            for c in todo.iter().flatten() {
                apply(&mut pcm[ch][..len], &orig[..len], &self.hist[ch][..self.hist_len], c);

                // Carry over what lies beyond this frame.
                if c.loc > len as i64 {
                    self.open[ch][next] = Some(Correction { loc: c.loc - len as i64, len: c.len });
                    next += 1;
                }
            }

            self.hist[ch][..len].copy_from_slice(&orig[..len]);
        }

        self.hist_len = len;
    }
}

/// The fade-in window of `length` samples: zero to unity. Together with its time reverse it
/// forms a pair of constant amplitude cross-fade windows.
fn fade_in(k: i64, length: i64) -> f32 {
    0.5 * (1.0 - (std::f32::consts::PI * (k as f32 + 0.5) / length as f32).cos())
}

/// Applies one correction to the frame `pcm`. `orig` is the unmodified frame and `hist` the
/// unmodified previous frame.
fn apply(pcm: &mut [f32], orig: &[f32], hist: &[f32], c: &Correction) {
    let off = hist.len() as i64;
    let len = orig.len() as i64;

    // The audio of the correction is addressed relative to the start of the previous frame.
    let transloc = off + c.loc;

    // The pre-noise starts with the audio block before the one that holds the transient.
    let pnlen = TC1 + c.loc.rem_euclid(TC1);
    let tot_corr_len = pnlen + c.len + TC1;

    let sample = |idx: i64| -> Option<f32> {
        if idx < 0 || idx >= off + len {
            None
        }
        else if idx < off {
            Some(hist[idx as usize])
        }
        else {
            Some(orig[(idx - off) as usize])
        }
    };

    let start = transloc - tot_corr_len;
    let synth_start = transloc - (2 * TC1 + 2 * pnlen);

    for s in 0..tot_corr_len {
        let dst = start + s;

        // Only the frame being decoded may still be modified.
        if dst < off || dst >= off + len {
            continue;
        }

        let (Some(synth), Some(cur)) = (sample(synth_start + s), sample(dst))
        else {
            continue;
        };

        pcm[(dst - off) as usize] = if s < TC1 {
            let fi = fade_in(s, TC1);
            cur * (1.0 - fi) + synth * fi
        }
        else if s < tot_corr_len - TC2 {
            synth
        }
        else {
            let fi = fade_in(s - (tot_corr_len - TC2), TC2);
            cur * fi + synth * (1.0 - fi)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_pre_noise_is_replaced_by_earlier_audio() {
        // A frame of "clean" tone with loud noise inserted before a transient at sample 1024.
        let mut pcm = vec![[0.0f32; FRAME_SIZE]; 1];
        let len = 1536;
        for (i, v) in pcm[0][..len].iter_mut().enumerate() {
            *v = 0.1 * ((i as f32) * 0.05).sin();
        }
        let clean = pcm[0];
        for v in pcm[0][600..1024].iter_mut() {
            *v += 0.5;
        }

        let mut pn = PreNoise::new();
        pn.hist_len = 0;
        let mut data: FrameData = [None; MAX_FBW];
        data[0] = Some((256, 8)); // transloc = 1024.
        pn.process_frame(&mut pcm, 1, len, &data);

        // Samples well inside the corrected region carry no more noise offset.
        let pnlen = 256;
        let start = 1024 - (pnlen + 8 + 256);
        for i in start + 256..1024 - 128 {
            assert!((pcm[0][i] - clean[i]).abs() < 0.5, "sample {i} not corrected");
            assert!(pcm[0][i].abs() < 0.2, "sample {i} still contains the noise");
        }
        // Samples after the transient and before the correction are untouched.
        assert_eq!(pcm[0][1200], clean[1200]);
        assert_eq!(pcm[0][start - 1], clean[start - 1]);
    }
}
