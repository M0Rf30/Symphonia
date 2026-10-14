// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nellymoser Asao decoder.
//!
//! Nellymoser Asao is a proprietary mono transform codec used by Flash (FLV) at 8, 11.025, 16,
//! 22.05 and 44.1 kHz. There is no public specification; this decoder follows the structure of the
//! FFmpeg `nellymoser` decoder (see `NOTICE`).
//!
//! Every 64-byte block holds 256 samples (two overlapped 128-coefficient MDCT frames). A packet
//! holds one or more whole blocks (no upper limit); any trailing partial block is ignored.

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
use symphonia_core::codecs::audio::well_known::CODEC_ID_NELLYMOSER;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderRtl, ReadBitsRtl};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{BAND_SIZES, DELTA_TABLE, DEQUANT_TABLE, INIT_TABLE, LFG_INITIAL_STATE};

const NELLY_BANDS: usize = 23;
/// Size of a block in bytes.
const NELLY_BLOCK_LEN: usize = 64;
const NELLY_HEADER_BITS: u32 = 116;
const NELLY_DETAIL_BITS: i32 = 198;
/// Number of MDCT coefficients per frame.
const NELLY_BUF_LEN: usize = 128;
/// Number of coded (non-zero) coefficients per frame.
const NELLY_FILL_LEN: usize = 124;
const NELLY_BIT_CAP: i32 = 6;
const NELLY_BASE_OFF: i32 = 4228;
const NELLY_BASE_SHIFT: i32 = 19;
/// Number of output samples per block.
const NELLY_SAMPLES: usize = 2 * NELLY_BUF_LEN;

/// Number of blocks the output buffer is initially sized for. Larger packets grow it.
const INITIAL_BLOCKS: usize = 16;

/// Scale applied to dequantized values: 1 / (32768 * 8).
const SCALE_BIAS: f64 = 1.0 / (32768.0 * 8.0);

/// Shift `i` left by `shift` bits if positive, otherwise arithmetic shift right by `-shift`.
/// Out-of-range shifts saturate rather than being undefined.
fn signed_shift(i: i32, shift: i32) -> i32 {
    if shift > 0 {
        if shift >= 32 { 0 } else { ((i as u32) << shift) as i32 }
    }
    else {
        i >> (-shift).min(31)
    }
}

fn log2_floor(v: u32) -> i32 {
    31 - v.leading_zeros() as i32
}

/// Normalizes `*la` so that its magnitude has its top bit at position 30. Returns the shift.
fn headroom(la: &mut i32) -> i32 {
    if *la == 0 {
        return 31;
    }
    let l = 30 - log2_floor(la.unsigned_abs());
    *la = (*la).wrapping_shl(l as u32);
    l
}

fn sum_bits(buf: &[i16; NELLY_FILL_LEN], shift: i32, off: i32) -> i32 {
    let mut ret = 0;
    for &s in buf.iter() {
        let b = i32::from(s).wrapping_sub(off);
        let b = ((b >> (shift - 1).clamp(0, 31)) + 1) >> 1;
        ret += b.clamp(0, NELLY_BIT_CAP);
    }
    ret
}

/// Computes the per-coefficient bit allocation from the band energies.
fn get_sample_bits(buf: &[i32; NELLY_FILL_LEN], bits: &mut [i32; NELLY_BUF_LEN]) {
    let mut sbuf = [0i16; NELLY_FILL_LEN];

    let mut max = buf.iter().copied().fold(0, i32::max);
    let mut shift = -16 + headroom(&mut max);

    let mut sum = 0i32;
    for (s, &b) in sbuf.iter_mut().zip(buf.iter()) {
        *s = signed_shift(b, shift) as i16;
        *s = ((3 * i32::from(*s)) >> 2) as i16;
        sum = sum.wrapping_add(i32::from(*s));
    }

    shift += 11;
    let shift_saved = shift;
    sum = sum.wrapping_sub(NELLY_DETAIL_BITS << shift.clamp(0, 31));
    shift += headroom(&mut sum);
    let mut small_off = (NELLY_BASE_OFF.wrapping_mul(sum >> 16)) >> 15;
    shift = shift_saved - (NELLY_BASE_SHIFT + shift - 31);

    small_off = signed_shift(small_off, shift);

    let mut bitsum = sum_bits(&sbuf, shift_saved, small_off);

    if bitsum != NELLY_DETAIL_BITS {
        let mut off = bitsum - NELLY_DETAIL_BITS;

        shift = 0;
        while off.abs() <= 16383 {
            off *= 2;
            shift += 1;
        }

        off = (off * NELLY_BASE_OFF) >> 15;
        shift = shift_saved - (NELLY_BASE_SHIFT + shift - 15);

        off = signed_shift(off, shift);

        let mut last_off = small_off;
        let mut last_bitsum = bitsum;
        let mut j = 1;
        while j < 20 {
            last_off = small_off;
            small_off = small_off.wrapping_add(off);
            last_bitsum = bitsum;

            bitsum = sum_bits(&sbuf, shift_saved, small_off);

            if (bitsum - NELLY_DETAIL_BITS) * (last_bitsum - NELLY_DETAIL_BITS) <= 0 {
                break;
            }
            j += 1;
        }

        let (mut big_off, mut big_bitsum, mut small_bitsum);
        if bitsum > NELLY_DETAIL_BITS {
            big_off = small_off;
            small_off = last_off;
            big_bitsum = bitsum;
            small_bitsum = last_bitsum;
        }
        else {
            big_off = last_off;
            big_bitsum = last_bitsum;
            small_bitsum = bitsum;
        }

        while bitsum != NELLY_DETAIL_BITS && j <= 19 {
            let off = (big_off.wrapping_add(small_off)) >> 1;
            bitsum = sum_bits(&sbuf, shift_saved, off);
            if bitsum > NELLY_DETAIL_BITS {
                big_off = off;
                big_bitsum = bitsum;
            }
            else {
                small_off = off;
                small_bitsum = bitsum;
            }
            j += 1;
        }

        if (big_bitsum - NELLY_DETAIL_BITS).abs() >= (small_bitsum - NELLY_DETAIL_BITS).abs() {
            bitsum = small_bitsum;
        }
        else {
            small_off = big_off;
            bitsum = big_bitsum;
        }
    }

    for (b, &s) in bits.iter_mut().zip(sbuf.iter()) {
        let tmp = i32::from(s).wrapping_sub(small_off);
        let tmp = ((tmp >> (shift_saved - 1).clamp(0, 31)) + 1) >> 1;
        *b = tmp.clamp(0, NELLY_BIT_CAP);
    }

    if bitsum > NELLY_DETAIL_BITS {
        let mut tmp = 0;
        let mut i = 0;
        while tmp < NELLY_DETAIL_BITS && i < NELLY_FILL_LEN {
            tmp += bits[i];
            i += 1;
        }

        if i > 0 {
            bits[i - 1] -= tmp - NELLY_DETAIL_BITS;
        }
        for b in &mut bits[i..NELLY_FILL_LEN] {
            *b = 0;
        }
    }
}

/// FFmpeg's additive lagged-Fibonacci generator (`av_lfg`), seeded with 0.
struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    fn new() -> Self {
        Lfg { state: LFG_INITIAL_STATE, index: 0 }
    }

    fn next(&mut self) -> u32 {
        let a = self.state[(self.index.wrapping_sub(24) & 63) as usize]
            .wrapping_add(self.state[(self.index.wrapping_sub(55) & 63) as usize]);
        self.state[(self.index & 63) as usize] = a;
        self.index = self.index.wrapping_add(1);
        a
    }
}

/// Block decoder state.
struct Core {
    imdct: Imdct,
    lfg: Lfg,
    /// IMDCT output of the previous frame.
    prev: [f32; NELLY_BUF_LEN],
    /// Sine overlap window.
    window: [f32; NELLY_BUF_LEN],
    scratch: [f32; 2 * NELLY_BUF_LEN],
}

impl Core {
    fn new() -> Self {
        let mut window = [0f32; NELLY_BUF_LEN];
        for (i, w) in window.iter_mut().enumerate() {
            *w = ((i as f64 + 0.5) / (2.0 * NELLY_BUF_LEN as f64) * std::f64::consts::PI).sin()
                as f32;
        }
        Core {
            imdct: Imdct::new(NELLY_BUF_LEN),
            lfg: Lfg::new(),
            prev: [0.0; NELLY_BUF_LEN],
            window,
            scratch: [0.0; 2 * NELLY_BUF_LEN],
        }
    }

    fn decode_block(&mut self, block: &[u8], audio: &mut [f32]) -> Result<()> {
        let mut bs = BitReaderRtl::new(block);

        let mut buf = [0i32; NELLY_FILL_LEN];
        let mut pows = [0f32; NELLY_FILL_LEN];

        let mut pos = 0;
        let mut val = INIT_TABLE[bs.read_bits_leq32(6)? as usize];
        for (i, &size) in BAND_SIZES.iter().enumerate() {
            if i > 0 {
                val += DELTA_TABLE[bs.read_bits_leq32(5)? as usize];
            }
            let pval = (-(f64::from(val as f32 / 2048.0)).exp2() * SCALE_BIAS) as f32;
            for _ in 0..size {
                buf[pos] = val;
                pows[pos] = pval;
                pos += 1;
            }
        }
        debug_assert_eq!(pos, NELLY_FILL_LEN);
        debug_assert_eq!(BAND_SIZES.len(), NELLY_BANDS);

        let mut bits = [0i32; NELLY_BUF_LEN];
        get_sample_bits(&buf, &mut bits);

        for i in 0..2 {
            let aptr = &mut audio[i * NELLY_BUF_LEN..(i + 1) * NELLY_BUF_LEN];

            let mut bs = BitReaderRtl::new(block);
            bs.ignore_bits(NELLY_HEADER_BITS + i as u32 * NELLY_DETAIL_BITS as u32)?;

            for j in 0..NELLY_FILL_LEN {
                if bits[j] <= 0 {
                    let v = (std::f64::consts::FRAC_1_SQRT_2 * f64::from(pows[j])) as f32;
                    aptr[j] = if self.lfg.next() & 1 != 0 { -v } else { v };
                }
                else {
                    let v = bs.read_bits_leq32(bits[j] as u32)? as usize;
                    aptr[j] = DEQUANT_TABLE[(1 << bits[j]) - 1 + v] * pows[j];
                }
            }
            for a in &mut aptr[NELLY_FILL_LEN..] {
                *a = 0.0;
            }

            // The full IMDCT yields 2N samples; the middle N are the unique ones.
            self.imdct.imdct(aptr, &mut self.scratch);
            // Symphonia's IMDCT has the opposite sign convention to the reference decoder.
            for v in &mut self.scratch {
                *v = -*v;
            }
            let cur = &self.scratch[NELLY_BUF_LEN / 2..NELLY_BUF_LEN / 2 + NELLY_BUF_LEN];

            // Windowed overlap-add with the previous frame.
            let half = NELLY_BUF_LEN / 2;
            for k in 0..half {
                let i0 = half + k; // index into prev (second half)
                let j = half - 1 - k;
                let s0 = self.prev[i0];
                let s1 = cur[j];
                let wi = self.window[k];
                let wj = self.window[half + j];
                aptr[k] = s0 * wj - s1 * wi;
                aptr[half + j] = s0 * wi + s1 * wj;
            }

            self.prev.copy_from_slice(cur);
        }

        Ok(())
    }
}

/// Nellymoser Asao decoder.
pub struct NellymoserDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    core: Core,
}

impl NellymoserDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_NELLYMOSER {
            return unsupported_error("nellymoser: invalid codec");
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("nellymoser: sample rate is required"),
        };

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("nellymoser: only mono is supported");
            }
        }

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_BLOCKS * NELLY_SAMPLES);

        let mut params = params.clone();
        params.with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(NellymoserDecoder { params, opts: *opts, buf, core: Core::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let blocks = packet.data.len() / NELLY_BLOCK_LEN;

        if blocks == 0 {
            return decode_error("nellymoser: packet is too small");
        }

        // Like the reference decoder, decode every whole block in the packet.
        if blocks * NELLY_SAMPLES > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), blocks * NELLY_SAMPLES);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(blocks * NELLY_SAMPLES));

        let out = self.buf.plane_mut(0).expect("mono plane");
        for (block, audio) in
            packet.data.chunks_exact(NELLY_BLOCK_LEN).zip(out.chunks_exact_mut(NELLY_SAMPLES))
        {
            self.core.decode_block(block, audio)?;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for NellymoserDecoder {
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

impl RegisterableAudioDecoder for NellymoserDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(NellymoserDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_NELLYMOSER, "nellymoser", "Nellymoser Asao")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::audio::Audio;

    fn decoder() -> NellymoserDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_NELLYMOSER).with_sample_rate(22050);
        NellymoserDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    /// Arbitrary (corrupt) input must never panic and must always yield finite output.
    #[test]
    fn verify_random_blocks_do_not_panic() {
        let mut dec = decoder();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut data = vec![0u8; NELLY_BLOCK_LEN * 3 + 17];
        for _ in 0..4000 {
            for b in data.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 32) as u8;
            }
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let buf = dec.decode_ref(&pkt).unwrap();
            if let GenericAudioBufferRef::F32(b) = buf {
                assert_eq!(b.frames(), 3 * NELLY_SAMPLES);
            }
        }
    }

    #[test]
    fn verify_short_packet_is_error() {
        let mut dec = decoder();
        let data = [0u8; NELLY_BLOCK_LEN - 1];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    #[test]
    fn verify_all_zero_block() {
        let mut dec = decoder();
        let data = [0u8; NELLY_BLOCK_LEN];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_ok());
    }
}
