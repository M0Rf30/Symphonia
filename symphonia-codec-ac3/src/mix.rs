// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Down-mix of decoded channels to stereo or mono, following ETSI TS 102 366 / ATSC A/52 clause
//! 7.8.2 (Lo/Ro and Lt/Rt) and Annex E clause 3.9 (LFE mix level).
//!
//! The mix is linear, so it is applied to the decoded PCM instead of to the transform
//! coefficients.

use symphonia_core::codecs::audio::Downmix;

use crate::header::MixLevels;
use crate::{
    LOC_C, LOC_CS, LOC_L, LOC_LFE, LOC_LRS, LOC_LS, LOC_R, LOC_RRS, LOC_RS, MAX_AU_CHANNELS,
};

/// The gain of a channel into the left and right outputs.
pub type Gain = (f32, f32);

/// The gain of a single surround channel in the Lo/Ro down-mix (clause 7.8.2: `0.7 * slev`).
const SINGLE_SURROUND: f32 = 0.7;

/// The gains of the channel at `loc` before normalization, or `None` if the location is not
/// part of the down-mix.
///
/// The surround locations beyond the left and right surround channels of a 5.1 programme (the
/// rear surround pair and the back center of 6.1 and 7.1 programmes) are mixed like surround
/// channels; the specification does not define the down-mix of those programmes. Other
/// locations (Lc/Rc, Ts, Lsd/Rsd, Lw/Rw, Vhl/Vhr, Vhc, Lts/Rts, LFE2) are not mixed.
fn channel_gain(mode: Downmix, mix: &MixLevels, loc: u8) -> Option<Gain> {
    let ltrt = mode == Downmix::StereoLtRt;

    Some(match loc {
        LOC_L => (1.0, 0.0),
        LOC_R => (0.0, 1.0),
        LOC_C if ltrt => (mix.ltrt_center, mix.ltrt_center),
        LOC_C => (mix.loro_center, mix.loro_center),
        // Lt = ... - slev * (Ls + Rs); Rt = ... + slev * (Ls + Rs).
        LOC_LS | LOC_RS | LOC_LRS | LOC_RRS if ltrt => (-mix.ltrt_surround, mix.ltrt_surround),
        LOC_LS | LOC_LRS => (mix.loro_surround, 0.0),
        LOC_RS | LOC_RRS => (0.0, mix.loro_surround),
        LOC_CS if ltrt => (-mix.ltrt_surround, mix.ltrt_surround),
        LOC_CS => (SINGLE_SURROUND * mix.loro_surround, SINGLE_SURROUND * mix.loro_surround),
        LOC_LFE => {
            let g = mix.lfe?;
            (g, g)
        }
        _ => return None,
    })
}

/// Computes the gains from every channel into the output of the down-mix, for the channels at
/// `locs`. Returns the number of output channels, 2 (left and right) or 1 (mono), with the mono
/// output in the left gain. Channels that are not mixed get a gain of zero.
///
/// The gains are scaled so that the gains into any one output sum to at most 1 (clause 7.8.1).
/// A mono down-mix is `(Lo + Ro) / 2` (clause 7.8.2).
pub fn gains(
    mode: Downmix,
    mix: &MixLevels,
    locs: &[u8],
    out: &mut [Gain; MAX_AU_CHANNELS],
) -> usize {
    // A mono down-mix is derived from Lo/Ro.
    let eff = if mode == Downmix::Mono { Downmix::StereoLoRo } else { mode };

    // A mono programme has only a center channel, which is output at unity gain.
    let sole_center = locs.iter().filter(|&&l| l != LOC_LFE).eq([LOC_C].iter());

    let (mut sum_l, mut sum_r) = (0.0f32, 0.0f32);
    for (g, &loc) in out.iter_mut().zip(locs) {
        *g = if sole_center && loc == LOC_C {
            (1.0, 1.0)
        }
        else {
            channel_gain(eff, mix, loc).unwrap_or((0.0, 0.0))
        };
        sum_l += g.0.abs();
        sum_r += g.1.abs();
    }

    let norm = sum_l.max(sum_r);
    let norm = if norm > 0.0 { 1.0 / norm } else { 1.0 };

    if mode == Downmix::Mono {
        for g in out.iter_mut().take(locs.len()) {
            *g = (0.5 * (g.0 + g.1) * norm, 0.0);
        }
        1
    }
    else {
        for g in out.iter_mut().take(locs.len()) {
            *g = (g.0 * norm, g.1 * norm);
        }
        2
    }
}
