// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sound Blaster Pro ADPCM: the 4-bit, 2.6-bit (3-bit) and 2-bit codecs of the Creative Voice
//! File (codec numbers 1, 2 and 3 of the sound data block).
//!
//! Matches FFmpeg's `adpcm_sbpro_4`, `adpcm_sbpro_3` and `adpcm_sbpro_2` decoders
//! (`adpcm_sbpro_expand_nibble` and the `AV_CODEC_ID_ADPCM_SBPRO_*` cases of `libavcodec/adpcm.c`).
//! Only mono streams are decoded.
//!
//! The first byte of the stream is a raw unsigned 8-bit sample, and the following bytes hold 2
//! (4-bit), 3 (2.6-bit), or 4 (2-bit) codes each, most significant first. As in FFmpeg, the
//! predictor and the step carry over from packet to packet, and the first byte after the decoder
//! was created or reset is the raw sample.

/// The variants of the codec.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// 4-bit codes: 2 samples per byte.
    Bits4,
    /// 2.6-bit codes (3, 3, and 2 bits): 3 samples per byte.
    Bits3,
    /// 2-bit codes: 4 samples per byte.
    Bits2,
}

impl Kind {
    fn samples_per_byte(self) -> usize {
        match self {
            Kind::Bits4 => 2,
            Kind::Bits3 => 3,
            Kind::Bits2 => 4,
        }
    }
}

pub(crate) struct SbproDecoder {
    kind: Kind,
    predictor: i32,
    step: i32,
    /// Set once the raw first sample has been read.
    started: bool,
}

impl SbproDecoder {
    pub(crate) fn new(kind: Kind) -> Self {
        SbproDecoder { kind, predictor: 0, step: 0, started: false }
    }

    pub(crate) fn reset(&mut self) {
        *self = SbproDecoder::new(self.kind);
    }

    /// The number of frames that `len` bytes decode to.
    pub(crate) fn frames_in(&self, len: usize) -> usize {
        if self.started {
            len * self.kind.samples_per_byte()
        }
        else {
            // The raw sample.
            len.saturating_sub(1) * self.kind.samples_per_byte() + usize::from(len > 0)
        }
    }

    /// Decodes a code of `size` bits, scaled by `shift` bits.
    fn expand(&mut self, code: u8, size: u32, shift: u32) -> i32 {
        let sign = code & (1 << (size - 1)) != 0;
        let delta = i32::from(code & ((1 << (size - 1)) - 1));

        let diff = delta << (7 + self.step + shift as i32);

        self.predictor = (self.predictor + if sign { -diff } else { diff }).clamp(-16384, 16256);

        if delta >= (2 * size as i32 - 3) && self.step < 3 {
            self.step += 1;
        }
        else if delta == 0 && self.step > 0 {
            self.step -= 1;
        }

        self.predictor << 16
    }

    /// Decodes `data` to `out`, which holds `frames_in(data.len())` frames.
    pub(crate) fn decode(&mut self, data: &[u8], out: &mut [i32]) {
        let mut data = data;
        let mut out = out;

        if !self.started {
            if let Some((&first, rest)) = data.split_first() {
                // The raw sample is not an input of the predictor.
                if let Some((o, rest_out)) = core::mem::take(&mut out).split_first_mut() {
                    *o = ((i32::from(first) - 0x80) * 128) << 16;
                    out = rest_out;
                }

                data = rest;
                self.started = true;
            }
        }

        let per_byte = self.kind.samples_per_byte();

        for (&byte, out) in data.iter().zip(out.chunks_exact_mut(per_byte)) {
            match self.kind {
                Kind::Bits4 => {
                    out[0] = self.expand(byte >> 4, 4, 0);
                    out[1] = self.expand(byte & 0x0f, 4, 0);
                }
                Kind::Bits3 => {
                    out[0] = self.expand(byte >> 5, 3, 0);
                    out[1] = self.expand((byte >> 2) & 0x07, 3, 0);
                    out[2] = self.expand(byte & 0x03, 2, 0);
                }
                Kind::Bits2 => {
                    out[0] = self.expand(byte >> 6, 2, 2);
                    out[1] = self.expand((byte >> 4) & 0x03, 2, 2);
                    out[2] = self.expand((byte >> 2) & 0x03, 2, 2);
                    out[3] = self.expand(byte & 0x03, 2, 2);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_frame_count() {
        let mut dec = SbproDecoder::new(Kind::Bits4);
        assert_eq!(dec.frames_in(0), 0);
        assert_eq!(dec.frames_in(5), 9);

        let mut out = [0; 9];
        dec.decode(&[0x80, 0x00, 0x00, 0x00, 0x00], &mut out);
        assert_eq!(out[0], 0);

        // After the raw sample: 2 samples per byte.
        assert_eq!(dec.frames_in(5), 10);

        dec.reset();
        assert_eq!(dec.frames_in(5), 9);
    }

    #[test]
    fn verify_raw_sample_and_steps() {
        let mut dec = SbproDecoder::new(Kind::Bits4);
        let mut out = [0; 3];

        // 0xff is a raw sample of +127 (127 * 128 = 16256); the code 0x7 is delta 7: the
        // predictor is 0 + (7 << 7) = 896, and the step goes to 1; the code 0xf is -7 at step 1:
        // 896 - (7 << 8) = -896.
        dec.decode(&[0xff, 0x7f], &mut out);
        assert_eq!(out, [16256 << 16, 896 << 16, -896 << 16]);
    }
}
