// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSP Group TrueSpeech decoder.
//!
//! TrueSpeech is a proprietary 8 kHz mono speech codec. Every 32-byte block holds one frame of 240
//! samples (four sub-frames of 60 samples). There is no public specification; this decoder is a
//! port of the FFmpeg `truespeech` decoder (see `NOTICE`) and is integer-exact with it.
//!
//! A packet holds one or more whole blocks (no upper limit); any trailing partial block is ignored.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TRUESPEECH;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{
    CODEBOOK, DECAY_3_4, DECAY_35_64, DECAY_994_1000, ORDER2_COEFFS, PULSE_SCALES, PULSE_VALUES,
};

/// Size of a block in bytes.
const TS_BLOCK_LEN: usize = 32;
/// Number of samples per block.
const TS_BLOCK_SAMPLES: usize = 240;
/// Number of samples per sub-frame.
const TS_SUBFRAME: usize = 60;
/// The sample rate used when the container does not state one.
const TS_SAMPLE_RATE: u32 = 8000;
/// Number of blocks the output buffer is initially sized for. Larger packets grow it.
const INITIAL_BLOCKS: usize = 16;

/// Clips to the range used by the reference decoder (`av_clip(x, -0x7FFE, 0x7FFE)`).
fn clip(v: i32) -> i16 {
    v.clamp(-0x7FFE, 0x7FFE) as i16
}

/// MSB-first reader over the byte-swapped block.
struct Bits {
    buf: [u8; TS_BLOCK_LEN],
    pos: usize,
}

impl Bits {
    /// The reference decoder byte-swaps each 32-bit word of the block, then reads MSB first.
    fn new(block: &[u8]) -> Self {
        let mut buf = [0u8; TS_BLOCK_LEN];
        for (dst, src) in buf.chunks_exact_mut(4).zip(block.chunks_exact(4)) {
            dst[0] = src[3];
            dst[1] = src[2];
            dst[2] = src[1];
            dst[3] = src[0];
        }
        Bits { buf, pos: 0 }
    }

    fn get(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.buf.get(self.pos >> 3).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        v
    }
}

/// Parsed frame parameters (`truespeech_read_frame`).
#[derive(Default)]
struct Frame {
    vector: [i16; 8],
    offset1: [i32; 2],
    offset2: [i32; 4],
    pulseoff: [usize; 4],
    pulsepos: [i32; 4],
    pulseval: [u32; 4],
    flag: bool,
}

impl Frame {
    fn read(block: &[u8]) -> Self {
        let mut gb = Bits::new(block);
        let mut f = Frame::default();

        // Vector 7 down to 0, with 3/3/3/4/4/4/5/5 bits.
        for (i, bits) in [(7, 3), (6, 3), (5, 3), (4, 4), (3, 4), (2, 4), (1, 5), (0, 5)] {
            let idx = gb.get(bits) as usize;
            f.vector[i] = CODEBOOK[i].get(idx).copied().unwrap_or(0);
        }
        f.flag = gb.get(1) != 0;

        f.offset1[0] = (gb.get(4) << 4) as i32;
        for i in (0..4).rev() {
            f.offset2[i] = gb.get(7) as i32;
        }

        f.offset1[1] = gb.get(4) as i32;
        f.pulseval[1] = gb.get(14);
        f.pulseval[0] = gb.get(14);

        f.offset1[1] |= (gb.get(4) << 4) as i32;
        f.pulseval[3] = gb.get(14);
        f.pulseval[2] = gb.get(14);

        for i in 0..4 {
            f.offset1[0] |= (gb.get(1) as i32) << i;
            f.pulsepos[i] = gb.get(27) as i32;
            f.pulseoff[i] = gb.get(4) as usize;
        }
        f
    }
}

/// Decoder state (`TSContext`).
struct Core {
    /// Excitation history.
    filtbuf: [i32; 146],
    /// Filter of the previous frame.
    prevfilt: [i32; 8],
    tmp1: [i16; 8],
    tmp2: [i16; 8],
    tmp3: [i16; 8],
    /// Correlated input vector.
    cvector: [i16; 8],
    /// Gain value used by the last synthesis stage.
    filtval: i32,
    newvec: [i16; TS_SUBFRAME],
    /// Filters for every sub-frame.
    filters: [i16; 32],
}

impl Core {
    fn new() -> Self {
        Core {
            filtbuf: [0; 146],
            prevfilt: [0; 8],
            tmp1: [0; 8],
            tmp2: [0; 8],
            tmp3: [0; 8],
            cvector: [0; 8],
            filtval: 0,
            newvec: [0; TS_SUBFRAME],
            filters: [0; 32],
        }
    }

    fn correlate_filter(&mut self, f: &Frame) {
        for i in 0..8 {
            if i > 0 {
                let tmp = self.cvector;
                for j in 0..i {
                    let add = (i32::from(tmp[i - j - 1]) * i32::from(f.vector[i]) + 0x4000) >> 15;
                    self.cvector[j] = i32::from(self.cvector[j]).wrapping_add(add) as i16;
                }
            }
            self.cvector[i] = ((8 - i32::from(f.vector[i])) >> 3) as i16;
        }
        for (i, &decay) in DECAY_994_1000.iter().enumerate() {
            self.cvector[i] = ((i32::from(self.cvector[i]) * i32::from(decay)) >> 15) as i16;
        }

        self.filtval = i32::from(f.vector[0]);
    }

    fn filters_merge(&mut self, f: &Frame) {
        for i in 0..8 {
            let (c, p) = (i32::from(self.cvector[i]), self.prevfilt[i]);
            if !f.flag {
                self.filters[i] = p as i16;
                self.filters[i + 8] = p as i16;
            }
            else {
                self.filters[i] = ((c * 21846 + p * 10923 + 16384) >> 15) as i16;
                self.filters[i + 8] = ((c * 10923 + p * 21846 + 16384) >> 15) as i16;
            }
            self.filters[i + 16] = self.cvector[i];
            self.filters[i + 24] = self.cvector[i];
        }
    }

    fn apply_twopoint_filter(&mut self, f: &Frame, quart: usize) {
        let mut t = f.offset2[quart];
        if t == 127 {
            self.newvec = [0; TS_SUBFRAME];
            return;
        }

        let mut tmp = [0i16; 146 + TS_SUBFRAME];
        for (d, &s) in tmp.iter_mut().zip(self.filtbuf.iter()) {
            *d = s as i16;
        }

        let off = ((t / 25) + f.offset1[quart >> 1] + 18).clamp(0, 145) as usize;
        let p0 = 145 - off;
        let coef = (t % 25) as usize * 2;
        let (c0, c1) = (i32::from(ORDER2_COEFFS[coef]), i32::from(ORDER2_COEFFS[coef + 1]));
        for i in 0..TS_SUBFRAME {
            let p0 = p0 + i;
            t = (i32::from(tmp[p0]) * c0)
                .wrapping_add(i32::from(tmp[p0 + 1]) * c1)
                .wrapping_add(0x2000)
                >> 14;
            self.newvec[i] = t as i16;
            tmp[146 + i] = t as i16;
        }
    }

    fn place_pulses(f: &mut Frame, out: &mut [i16], quart: usize) {
        out.fill(0);

        let mut tmp = [0i16; 7];
        for i in 0..7 {
            let t = (f.pulseval[quart] & 3) as usize;
            f.pulseval[quart] >>= 2;
            tmp[6 - i] = PULSE_SCALES[f.pulseoff[quart] * 4 + t];
        }

        let mut ptr2 = 0;
        let mut coef = f.pulsepos[quart] >> 15;
        let mut ptr1 = 30;
        let (mut i, mut j) = (0, 3);
        while i < 30 && j > 0 {
            let t = i32::from(PULSE_VALUES.get(ptr1).copied().unwrap_or(0));
            ptr1 += 1;
            if coef >= t {
                coef -= t;
            }
            else {
                out[i] = tmp.get(ptr2).copied().unwrap_or(0);
                ptr2 += 1;
                ptr1 += 30;
                j -= 1;
            }
            i += 1;
        }

        coef = f.pulsepos[quart] & 0x7FFF;
        ptr1 = 0;
        let (mut i, mut j) = (30, 4);
        while i < 60 && j > 0 {
            let t = i32::from(PULSE_VALUES.get(ptr1).copied().unwrap_or(0));
            ptr1 += 1;
            if coef >= t {
                coef -= t;
            }
            else {
                out[i] = tmp.get(ptr2).copied().unwrap_or(0);
                ptr2 += 1;
                ptr1 += 30;
                j -= 1;
            }
            i += 1;
        }
    }

    fn update_filters(&mut self, out: &mut [i16]) {
        self.filtbuf.copy_within(60..146, 0);
        for (i, o) in out.iter_mut().enumerate() {
            let nv = i32::from(self.newvec[i]);
            self.filtbuf[i + 86] = i32::from(*o) + nv - (nv >> 3);
            *o = (i32::from(*o) + nv) as i16;
        }
    }

    fn synth(&mut self, out: &mut [i16], quart: usize) {
        let ptr1 = &self.filters[quart * 8..quart * 8 + 8];

        // Short-term synthesis.
        for o in out.iter_mut() {
            let mut sum = 0i32;
            for (&h, &c) in self.tmp1.iter().zip(ptr1) {
                // The reference multiplies by an unsigned value, so this wraps.
                sum = sum.wrapping_add(i32::from(h).wrapping_mul(i32::from(c)));
            }
            let s = i32::from(*o) + (sum.wrapping_add(0x800) >> 12);
            *o = clip(s);
            self.tmp1.copy_within(0..7, 1);
            self.tmp1[0] = *o;
        }

        let mut t = [0i32; 8];
        for i in 0..8 {
            t[i] = (i32::from(DECAY_35_64[i]) * i32::from(ptr1[i])) >> 15;
        }

        for o in out.iter_mut() {
            let mut sum = 0i32;
            for (&h, &c) in self.tmp2.iter().zip(&t) {
                sum = sum.wrapping_add(i32::from(h).wrapping_mul(c));
            }
            self.tmp2.copy_within(0..7, 1);
            self.tmp2[0] = *o;
            *o = (i32::from(*o).wrapping_add(sum.wrapping_neg() >> 12)) as i16;
        }

        for i in 0..8 {
            t[i] = (i32::from(DECAY_3_4[i]) * i32::from(ptr1[i])) >> 15;
        }

        for o in out.iter_mut() {
            let mut sum = i32::from(*o) * (1 << 12);
            for (&h, &c) in self.tmp3.iter().zip(&t) {
                sum = sum.wrapping_add(i32::from(h).wrapping_mul(c));
            }
            self.tmp3.copy_within(0..7, 1);
            self.tmp3[0] = clip(sum.wrapping_add(0x800) >> 12);

            let gain = self.filtval - (self.filtval >> 2);
            sum = ((i32::from(self.tmp3[1]) * gain) >> 4).wrapping_add(sum);
            sum = sum.wrapping_sub(sum >> 3);
            *o = clip(sum.wrapping_add(0x800) >> 12);
        }
    }

    /// Decodes one 32-byte block into 240 samples.
    fn decode_block(&mut self, block: &[u8], out: &mut [i16]) {
        let mut f = Frame::read(block);

        self.correlate_filter(&f);
        self.filters_merge(&f);

        for (quart, sub) in out.chunks_exact_mut(TS_SUBFRAME).enumerate() {
            self.apply_twopoint_filter(&f, quart);
            Self::place_pulses(&mut f, sub, quart);
            self.update_filters(sub);
            self.synth(sub, quart);
        }

        for i in 0..8 {
            self.prevfilt[i] = i32::from(self.cvector[i]);
        }
    }
}

/// DSP Group TrueSpeech decoder.
pub struct TrueSpeechDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    core: Core,
}

impl TrueSpeechDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_TRUESPEECH {
            return unsupported_error("truespeech: invalid codec");
        }

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("truespeech: only mono is supported");
            }
        }

        let rate = params.sample_rate.filter(|&r| r > 0).unwrap_or(TS_SAMPLE_RATE);

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_BLOCKS * TS_BLOCK_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(TrueSpeechDecoder { params, opts: *opts, buf, core: Core::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let blocks = packet.data.len() / TS_BLOCK_LEN;

        if blocks == 0 {
            return decode_error("truespeech: packet is too small");
        }

        if blocks * TS_BLOCK_SAMPLES > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), blocks * TS_BLOCK_SAMPLES);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(blocks * TS_BLOCK_SAMPLES));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("truespeech: missing output plane");
        };

        for (block, audio) in
            packet.data.chunks_exact(TS_BLOCK_LEN).zip(out.chunks_exact_mut(TS_BLOCK_SAMPLES))
        {
            self.core.decode_block(block, audio);
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for TrueSpeechDecoder {
    fn reset(&mut self) {
        self.core = Core::new();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for TrueSpeechDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(TrueSpeechDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_TRUESPEECH, "truespeech", "DSP Group TrueSpeech")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> TrueSpeechDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_TRUESPEECH).with_sample_rate(8000);
        TrueSpeechDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    /// Arbitrary (corrupt) input must never panic.
    #[test]
    fn verify_random_blocks_do_not_panic() {
        let mut dec = decoder();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut data = vec![0u8; TS_BLOCK_LEN * 3 + 17];
        for _ in 0..4000 {
            for b in data.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 32) as u8;
            }
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let buf = dec.decode_ref(&pkt).unwrap();
            if let GenericAudioBufferRef::S16(b) = buf {
                assert_eq!(b.frames(), 3 * TS_BLOCK_SAMPLES);
            }
        }
    }

    #[test]
    fn verify_short_packet_is_error() {
        let mut dec = decoder();
        let data = [0u8; TS_BLOCK_LEN - 1];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
    }
}
