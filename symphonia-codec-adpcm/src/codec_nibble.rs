// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The headerless 4-bit ADPCM codecs of `WAVE`: Yamaha (AICA / YMZ280B style,
//! `WAVE_FORMAT_YAMAHA_ADPCM` 0x0020), Creative Technology (`WAVE_FORMAT_CREATIVE_ADPCM` 0x0200)
//! and Dialogic OKI (`WAVE_FORMAT_OKI_ADPCM` 0x0010 and 0x0017).
//!
//! Matches FFmpeg's `adpcm_yamaha`, `adpcm_ct` and `adpcm_ima_oki` decoders
//! (`adpcm_*_expand_nibble` in `libavcodec/adpcm.c`, with `ff_adpcm_yamaha_difflookup`,
//! `ff_adpcm_yamaha_indexscale`, `ff_adpcm_AdaptationTable`, `ff_adpcm_index_table` and
//! `oki_step_table`). A byte holds two codes. Mono data holds two frames per byte; stereo data
//! holds one frame per byte, where the first code of the byte is the left channel and the second
//! the right channel. Yamaha stores the first code in the low nibble; Creative and OKI in the high
//! nibble. The predictor and step carry over from packet to packet.

use symphonia_core::util::clamp::clamp_i16;

use crate::common::from_i16_shift;

/// The most channels. FFmpeg decodes up to 2; more follow the same sample interleaving.
pub(crate) const MAX_CHANNELS: usize = 8;

/// The variants of the headerless 4-bit codecs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Yamaha,
    Creative,
    Oki,
}

// Yamaha. Source: FFmpeg `ff_adpcm_yamaha_difflookup` and `ff_adpcm_yamaha_indexscale`.
const YAMAHA_DIFF_LOOKUP: [i32; 16] =
    [1, 3, 5, 7, 9, 11, 13, 15, -1, -3, -5, -7, -9, -11, -13, -15];
const YAMAHA_INDEX_SCALE: [i32; 16] =
    [230, 230, 230, 230, 307, 409, 512, 614, 230, 230, 230, 230, 307, 409, 512, 614];
const YAMAHA_STEP_MIN: i32 = 127;
const YAMAHA_STEP_MAX: i32 = 24576;

// Creative. Source: FFmpeg `ff_adpcm_AdaptationTable` (first 8 entries) and
// `adpcm_ct_expand_nibble`.
const CT_ADAPTATION: [i32; 8] = [230, 230, 230, 230, 307, 409, 512, 614];
const CT_STEP_INIT: i32 = 511;
const CT_STEP_MAX: i32 = 32767;

// OKI. Source: FFmpeg `oki_step_table` and `ff_adpcm_index_table`.
#[rustfmt::skip]
const OKI_STEP_TABLE: [i32; 49] = [
     16,  17,  19,  21,   23,   25,   28,   31,   34,  37,
     41,  45,  50,  55,   60,   66,   73,   80,   88,  97,
    107, 118, 130, 143,  157,  173,  190,  209,  230, 253,
    279, 307, 337, 371,  408,  449,  494,  544,  598, 658,
    724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552,
];
const OKI_INDEX_TABLE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// The adaptive state of one channel. `step` is the quantiser step, except for OKI where it is
/// the index into [`OKI_STEP_TABLE`].
#[derive(Clone, Copy)]
struct ChannelState {
    predictor: i32,
    step: i32,
}

impl ChannelState {
    const fn new(kind: Kind) -> Self {
        match kind {
            Kind::Yamaha => Self { predictor: 0, step: YAMAHA_STEP_MIN },
            Kind::Creative => Self { predictor: 0, step: CT_STEP_INIT },
            Kind::Oki => Self { predictor: 0, step: 0 },
        }
    }

    fn expand_yamaha(&mut self, nibble: u8) -> i32 {
        let nibble = usize::from(nibble);
        self.predictor =
            clamp_i16(self.predictor + (self.step * YAMAHA_DIFF_LOOKUP[nibble]) / 8) as i32;
        self.step =
            ((self.step * YAMAHA_INDEX_SCALE[nibble]) >> 8).clamp(YAMAHA_STEP_MIN, YAMAHA_STEP_MAX);
        self.predictor
    }

    fn expand_creative(&mut self, nibble: u8) -> i32 {
        let magnitude = usize::from(nibble & 7);
        let diff = ((2 * magnitude as i32 + 1) * self.step) >> 3;
        // The predictor decays by 254/256 before the difference is added.
        let predictor = ((self.predictor * 254) >> 8) + if nibble & 8 != 0 { -diff } else { diff };
        self.predictor = clamp_i16(predictor) as i32;
        self.step = ((CT_ADAPTATION[magnitude] * self.step) >> 8).clamp(CT_STEP_INIT, CT_STEP_MAX);
        self.predictor
    }

    fn expand_oki(&mut self, nibble: u8) -> i32 {
        let magnitude = usize::from(nibble & 7);
        let step = OKI_STEP_TABLE[self.step as usize];
        let diff = ((2 * magnitude as i32 + 1) * step) >> 3;
        let predictor = if nibble & 8 != 0 { self.predictor - diff } else { self.predictor + diff };
        // The predictor is 12 bits; the output is scaled to 16.
        self.predictor = predictor.clamp(-2048, 2047);
        self.step = (self.step + OKI_INDEX_TABLE[magnitude]).clamp(0, 48);
        self.predictor * 16
    }
}

pub(crate) struct NibbleDecoder {
    kind: Kind,
    state: [ChannelState; MAX_CHANNELS],
}

impl NibbleDecoder {
    pub(crate) fn new(kind: Kind) -> Self {
        Self { kind, state: [ChannelState::new(kind); MAX_CHANNELS] }
    }

    pub(crate) fn reset(&mut self) {
        self.state = [ChannelState::new(self.kind); MAX_CHANNELS];
    }

    /// Expands `nibble` for channel `ch` and returns a 16-bit sample in the upper half of an i32.
    fn expand(&mut self, ch: usize, nibble: u8) -> i32 {
        let state = &mut self.state[ch];
        let sample = match self.kind {
            Kind::Yamaha => state.expand_yamaha(nibble),
            Kind::Creative => state.expand_creative(nibble),
            Kind::Oki => state.expand_oki(nibble),
        };
        from_i16_shift!(sample)
    }

    /// Splits a byte into its first and second code.
    fn codes(&self, byte: u8) -> (u8, u8) {
        match self.kind {
            Kind::Yamaha => (byte & 0x0f, byte >> 4),
            Kind::Creative | Kind::Oki => (byte >> 4, byte & 0x0f),
        }
    }

    /// Decodes mono `data` (2 frames per byte) into `out`.
    pub(crate) fn decode_mono(&mut self, data: &[u8], out: &mut [i32]) {
        for (&byte, out) in data.iter().zip(out.chunks_exact_mut(2)) {
            let (first, second) = self.codes(byte);
            out[0] = self.expand(0, first);
            out[1] = self.expand(0, second);
        }
    }

    /// Decodes the codes of channel `ch` of `channels` (code `i * channels + ch` of the nibble
    /// stream, first code of a byte first) into `out`, which holds the frames of whole bytes.
    pub(crate) fn decode_channel(
        &mut self,
        data: &[u8],
        ch: usize,
        channels: usize,
        out: &mut [i32],
    ) {
        for (i, o) in out.iter_mut().enumerate() {
            let n = i * channels + ch;
            let Some(&byte) = data.get(n / 2)
            else {
                break;
            };
            let (first, second) = self.codes(byte);
            *o = self.expand(ch, if n % 2 == 0 { first } else { second });
        }
    }

    /// Decodes stereo `data` (1 frame per byte) into `left` and `right`.
    pub(crate) fn decode_stereo(&mut self, data: &[u8], left: &mut [i32], right: &mut [i32]) {
        for ((&byte, l), r) in data.iter().zip(left.iter_mut()).zip(right.iter_mut()) {
            let (first, second) = self.codes(byte);
            *l = self.expand(0, first);
            *r = self.expand(1, second);
        }
    }
}
