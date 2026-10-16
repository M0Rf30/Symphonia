// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio Lossless (RALF) decoder.
//!
//! RALF is the lossless codec of RealAudio 10. There is no public specification; this decoder
//! follows FFmpeg's `libavcodec/ralf.c` and `ralfdata.h` (see `NOTICE`), and its output is
//! bit-exact with that decoder.
//!
//! # Stream parameters
//!
//! `extra_data` must hold the RealMedia type-specific data of the stream, which starts with the
//! `LSD:` header (all fields big-endian):
//!
//! | Offset | Size | Field                                                          |
//! |--------|------|----------------------------------------------------------------|
//! | 0      | 4    | `LSD:`                                                         |
//! | 4      | 2    | Version, `0x0103`                                              |
//! | 8      | 2    | Number of channels, 1 or 2                                     |
//! | 12     | 4    | Sample rate in Hz, 8000 to 96000                               |
//! | 16     | 4    | Maximum number of samples per packet and channel, at most 2^20 |
//!
//! At least 24 bytes are required. The channel count and sample rate are taken from this header
//! only; those of the codec parameters are ignored, as in FFmpeg. The output is 16-bit planar
//! audio, front left then front right.
//!
//! # Packets
//!
//! A packet is the payload of one RealMedia data packet, unchanged. It holds a table of the
//! blocks it contains (the size of each block in bytes), followed by the blocks, each of 64 to
//! 4096 samples. Blocks never depend on one another, and neither do packets, with one exception:
//! a frame that does not fit a single packet is carried by two. The first of the two is exactly
//! 8192 bytes long and produces no output (an empty buffer is returned); the second repeats the
//! block table of the first and then continues the data of the first.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

mod bits;
mod tables;
mod vlc;

use log::warn;

use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_RALF;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bits::BitReader;
use tables::{
    BIAS_DEF, BIAS_ELEMENTS, CODING_MODE_DEF, CODING_MODE_ELEMENTS, FILTER_COEFFS_DEF,
    FILTER_COEFFS_ELEMENTS, FILTER_PARAM_DEF, FILTERPARAM_ELEMENTS, LONG_CODES_DEF,
    LONG_CODES_ELEMENTS, SHORT_CODES_DEF, SHORT_CODES_ELEMENTS,
};
use vlc::Vlc;

/// The filter parameter code of a block of zeros.
const FILTER_NONE: usize = 0;
/// The filter parameter code of a block of raw samples.
const FILTER_RAW: usize = 642;

/// The size of a packet that is the first half of a frame split in two.
const MAX_PKT_SIZE: usize = 8192;
/// The largest number of samples of a block.
const MAX_BLOCK_LEN: usize = 4096;
/// The largest number of blocks of a packet.
const MAX_BLOCKS: usize = 1 << 12;
/// The longest prediction filter.
const MAX_FILTER_LEN: usize = 64;
/// The largest valid `max_frame_size`.
const MAX_FRAME_SIZE_LIMIT: u32 = 1 << 20;

/// The size of the `LSD:` header.
const HEADER_SIZE: usize = 24;
/// The only known version of the `LSD:` header.
const HEADER_VERSION: u16 = 0x103;

/// The code tables of one of the three block types.
struct VlcSet {
    filter_params: Vlc,
    bias: Vlc,
    coding_mode: Vlc,
    /// Indexed by `filter_bits * 11 + coefficient context + 5`.
    filter_coeffs: Vec<Vlc>,
    short_codes: Vec<Vlc>,
    long_codes: Vec<Vlc>,
}

impl VlcSet {
    fn new(set: usize) -> Result<Self> {
        let mut filter_coeffs = Vec::with_capacity(10 * 11);

        for bits in &FILTER_COEFFS_DEF[set] {
            for table in bits {
                filter_coeffs.push(Vlc::new(table, FILTER_COEFFS_ELEMENTS)?);
            }
        }

        let short_codes = SHORT_CODES_DEF[set]
            .iter()
            .map(|table| Vlc::new(table, SHORT_CODES_ELEMENTS))
            .collect::<Result<Vec<_>>>()?;

        let long_codes = LONG_CODES_DEF[set]
            .iter()
            .map(|table| Vlc::new(table, LONG_CODES_ELEMENTS))
            .collect::<Result<Vec<_>>>()?;

        Ok(VlcSet {
            filter_params: Vlc::new(&FILTER_PARAM_DEF[set], FILTERPARAM_ELEMENTS)?,
            bias: Vlc::new(&BIAS_DEF[set], BIAS_ELEMENTS)?,
            coding_mode: Vlc::new(&CODING_MODE_DEF[set], CODING_MODE_ELEMENTS)?,
            filter_coeffs,
            short_codes,
            long_codes,
        })
    }
}

/// Reads a value coded as a symbol of an escape code (FFmpeg `extend_code`).
///
/// A `val` of 0 or `2 * range` is an escape followed by an Exp-Golomb code. Finally `bits` raw
/// low bits are appended.
fn extend_code(br: &mut BitReader<'_>, val: i32, range: i32, bits: u32) -> Result<i32> {
    let val = if val == 0 {
        -range - br.get_ue_golomb()? as i32
    }
    else if val == range * 2 {
        range + br.get_ue_golomb()? as i32
    }
    else {
        val - range
    };

    if bits == 0 { Ok(val) } else { Ok((((val as u32) << bits) | br.get_bits(bits)) as i32) }
}

/// The index of the highest set bit of `v`, which must not be 0 (FFmpeg `av_log2`).
fn log2_floor(v: u32) -> i32 {
    31 - v.leading_zeros() as i32
}

/// Adds the prediction of the filter to the samples of a block (FFmpeg `apply_lpc`).
///
/// The history of the first sample is empty. `filter_bits` is the precision of the coefficients
/// plus 3, and `bits` the width of the samples, which bounds the prediction.
fn apply_lpc(audio: &mut [i32], filter: &[i32], filter_bits: u32, bits: u32) {
    let round = 1i32 << (filter_bits - 1);
    let max_clip = (1i32 << bits) - 1;
    let min_clip = -max_clip - 1;

    for i in 1..audio.len() {
        let (history, rest) = audio.split_at_mut(i);

        let taps = filter.len().min(i);

        // The sum wraps like the reference's, whose products and sum are unsigned.
        let acc = filter[..taps]
            .iter()
            .zip(history[i - taps..].iter().rev())
            .fold(0i32, |acc, (&c, &s)| acc.wrapping_add(c.wrapping_mul(s)));

        let pred = if acc < 0 {
            (acc.wrapping_add(round).wrapping_sub(1) >> filter_bits).max(min_clip)
        }
        else {
            // The reference shifts the unsigned sum.
            (((acc as u32).wrapping_add(round as u32) >> filter_bits) as i32).min(max_clip)
        };

        rest[0] = rest[0].wrapping_add(pred);
    }
}

/// The decoder of the blocks of a frame.
struct Core {
    channels: usize,
    max_frame_size: usize,
    sets: Vec<VlcSet>,
    /// The decoded samples of each channel of the current block.
    channel_data: [Vec<i32>; 2],
    /// The prediction filter of the current channel, and its length and precision in bits.
    filter: [i32; MAX_FILTER_LEN],
    filter_length: usize,
    filter_bits: u32,
    /// The constant added to the samples of each channel of the current block.
    bias: [u32; 2],
    /// The size in bytes of each block of the packet.
    block_sizes: Vec<u16>,
}

impl Core {
    fn new(channels: usize, max_frame_size: usize) -> Result<Self> {
        let sets = (0..3).map(VlcSet::new).collect::<Result<Vec<_>>>()?;

        Ok(Core {
            channels,
            max_frame_size,
            sets,
            channel_data: [vec![0; MAX_BLOCK_LEN], vec![0; MAX_BLOCK_LEN]],
            filter: [0; MAX_FILTER_LEN],
            filter_length: 0,
            filter_bits: 0,
            bias: [0; 2],
            block_sizes: Vec::with_capacity(MAX_BLOCKS),
        })
    }

    /// Decodes the samples of a channel of a block into `channel_data[ch]`, using the code tables
    /// of the set `mode` and `bits` bit wide raw samples. Returns whether the prediction filter
    /// must be applied to them.
    fn decode_channel(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        length: usize,
        mode: usize,
        bits: u32,
    ) -> Result<bool> {
        let set = &self.sets[mode];
        let dst = &mut self.channel_data[ch][..length];

        let filter_params = set.filter_params.read(br)?;

        if filter_params == FILTER_RAW {
            for sample in dst.iter_mut() {
                *sample = br.get_bits(bits) as i32;
            }

            self.bias[ch] = 0;
            return Ok(false);
        }

        let bias = set.bias.read(br)? as i32;
        self.bias[ch] = extend_code(br, bias, 127, 4)? as u32;

        if filter_params == FILTER_NONE {
            dst.fill(0);
            return Ok(false);
        }

        // Filter parameters 1 stand for no filter, but coded samples.
        let has_filter = filter_params > 1;

        if has_filter {
            self.filter_bits = ((filter_params - 2) >> 6) as u32;
            self.filter_length = filter_params - ((self.filter_bits as usize) << 6) - 1;

            // The coefficients are coded relative to their predecessor, with the code selected
            // by the magnitude of the previous one.
            let coeff_vlcs = &set.filter_coeffs[self.filter_bits as usize * 11..][..11];
            let add_bits = self.filter_bits;

            let mut cmode = 0i32;
            let mut coeff = 0i32;

            for slot in &mut self.filter[..self.filter_length] {
                let t = coeff_vlcs[(cmode + 5) as usize].read(br)? as i32;
                let t = extend_code(br, t, 21, add_bits)?;

                if cmode == 0 {
                    coeff = coeff.wrapping_sub((12u32 << add_bits) as i32);
                }

                coeff = (t as u32).wrapping_sub(coeff as u32) as i32;
                *slot = coeff;

                cmode = coeff >> add_bits;

                if cmode < 0 {
                    cmode = (-1 - log2_floor(cmode.unsigned_abs())).max(-5);
                }
                else if cmode > 0 {
                    cmode = (1 + log2_floor(cmode as u32)).min(5);
                }
            }
        }

        let code_params = set.coding_mode.read(br)?;

        let (range, range2, add_bits, code_vlc) = if code_params >= 15 {
            let mut add_bits = ((code_params / 5 - 3) / 2).min(10) as u32;

            if add_bits > 9 && code_params % 5 != 2 {
                add_bits -= 1;
            }

            (10, 21, add_bits, &set.long_codes[code_params - 15])
        }
        else {
            (6, 13, 0, &set.short_codes[code_params])
        };

        for pair in dst.chunks_exact_mut(2) {
            let t = code_vlc.read(br)?;
            let code1 = (t / range2) as i32;
            let code2 = (t % range2) as i32;

            let mut v0 = (extend_code(br, code1, range, 0)? as u32) << add_bits;
            let mut v1 = (extend_code(br, code2, range, 0)? as u32) << add_bits;

            if add_bits != 0 {
                v0 |= br.get_bits(add_bits);
                v1 |= br.get_bits(add_bits);
            }

            pair[0] = v0 as i32;
            pair[1] = v1 as i32;
        }

        Ok(has_filter)
    }

    /// Decodes a block into `out0` and `out1` at `offset`, and returns its length in samples.
    fn decode_block(
        &mut self,
        br: &mut BitReader<'_>,
        offset: usize,
        out0: &mut [i16],
        out1: &mut [i16],
    ) -> Result<usize> {
        let mut len = 12 - br.get_unary(6);

        // The codes for the lengths 6 and 7 are swapped.
        if len <= 7 {
            len ^= 1;
        }

        let len = 1usize << len;

        if offset + len > self.max_frame_size.min(out0.len()) {
            return decode_error("ralf: block exceeds the maximum frame size");
        }

        // How the channels are coded: 0 for mono, else 1 to 4.
        let dmode = if self.channels > 1 { br.get_bits(2) as usize + 1 } else { 0 };

        let mode = [if dmode == 4 { 1 } else { 0 }, if dmode >= 2 { 2 } else { 0 }];
        let bits = [16, if mode[1] == 2 { 17 } else { 16 }];

        for ch in 0..self.channels {
            let has_filter = self.decode_channel(br, ch, len, mode[ch], bits[ch])?;

            if has_filter {
                self.filter_bits += 3;

                apply_lpc(
                    &mut self.channel_data[ch][..len],
                    &self.filter[..self.filter_length],
                    self.filter_bits,
                    bits[ch],
                );
            }

            if br.bits_left() < 0 {
                return decode_error("ralf: block overrun");
            }
        }

        let [ch0, ch1] = &mut self.channel_data;
        let ch0 = &mut ch0[..len];
        let ch1 = &ch1[..len];
        let [bias0, bias1] = self.bias;

        // Every sum wraps, and is truncated to 16 bits, like the reference's.
        let dst0 = &mut out0[offset..offset + len];

        match dmode {
            0 => {
                for (d, &s) in dst0.iter_mut().zip(ch0.iter()) {
                    *d = (s as u32).wrapping_add(bias0) as i16;
                }
            }
            1 => {
                let dst1 = &mut out1[offset..offset + len];

                for (((d0, d1), &s0), &s1) in dst0.iter_mut().zip(dst1).zip(ch0.iter()).zip(ch1) {
                    *d0 = (s0 as u32).wrapping_add(bias0) as i16;
                    *d1 = (s1 as u32).wrapping_add(bias1) as i16;
                }
            }
            2 => {
                let dst1 = &mut out1[offset..offset + len];

                for (((d0, d1), s0), &s1) in dst0.iter_mut().zip(dst1).zip(ch0.iter_mut()).zip(ch1)
                {
                    // Left, and left minus right.
                    let left = (*s0 as u32).wrapping_add(bias0);
                    *s0 = left as i32;
                    *d0 = left as i16;
                    *d1 = left.wrapping_sub((s1 as u32).wrapping_add(bias1)) as i16;
                }
            }
            3 => {
                let dst1 = &mut out1[offset..offset + len];

                for (((d0, d1), &s0), &s1) in dst0.iter_mut().zip(dst1).zip(ch0.iter()).zip(ch1) {
                    // Right, and left minus right.
                    let t = (s0 as u32).wrapping_add(bias0);
                    let t2 = (s1 as u32).wrapping_add(bias1);
                    *d0 = t.wrapping_add(t2) as i16;
                    *d1 = t as i16;
                }
            }
            _ => {
                let dst1 = &mut out1[offset..offset + len];

                for (((d0, d1), &s0), &s1) in dst0.iter_mut().zip(dst1).zip(ch0.iter()).zip(ch1) {
                    // Mid and side. The side's low bit is the mid's, which was dropped.
                    let t = (s1 as u32).wrapping_add(bias1);
                    let t2 = (s0 as u32).wrapping_add(bias0).wrapping_mul(2) | (t & 1);
                    *d0 = (t2.wrapping_add(t) as i32 / 2) as i16;
                    *d1 = (t2.wrapping_sub(t) as i32 / 2) as i16;
                }
            }
        }

        Ok(len)
    }

    /// Decodes the frame in `src`, a packet or two joined packets, into the planes `out0` and
    /// `out1`. Returns the number of samples per channel decoded.
    fn decode_frame(&mut self, src: &[u8], out0: &mut [i16], out1: &mut [i16]) -> Result<usize> {
        if src.len() < 5 {
            return decode_error("ralf: packet is too short");
        }

        // The table of blocks is `table_size` bits long and starts after its 2-byte size.
        let table_size = usize::from(u16::from_be_bytes([src[0], src[1]]));
        let table_bytes = (table_size + 7) >> 3;

        if src.len() < table_bytes + 3 {
            return decode_error("ralf: block table exceeds the packet");
        }

        // Like the reference, read the table from the whole of the data: an entry cut short by
        // the end of the table is completed from the bytes that follow.
        let mut table = BitReader::new(&src[2..], table_size);

        self.block_sizes.clear();

        while table.bits_left() > 0 {
            if self.block_sizes.len() >= MAX_BLOCKS {
                return decode_error("ralf: too many blocks");
            }

            self.block_sizes.push(table.get_bits(13 + self.channels as u32) as u16);

            // Skip the presentation time of the block (milliseconds), if any.
            if table.get_bit() {
                table.skip(9);
            }
        }

        let mut pos = 2 + table_bytes;
        let mut num_samples = 0;

        for i in 0..self.block_sizes.len() {
            let size = usize::from(self.block_sizes[i]);

            // The error is reported only if there is nothing to output, as FFmpeg decodes as
            // many blocks as it can.
            let result = match src.get(pos..pos + size) {
                Some(block) => {
                    let mut br = BitReader::new(block, size * 8);
                    self.decode_block(&mut br, num_samples, out0, out1)
                }
                None => decode_error("ralf: block exceeds the packet"),
            };

            match result {
                Ok(len) => num_samples += len,
                Err(err) if num_samples == 0 => return Err(err),
                Err(err) => {
                    warn!("ralf: dropping the rest of the packet: {err}");
                    break;
                }
            }

            pos += size;
        }

        Ok(num_samples)
    }
}

/// The parameters of an `LSD:` header.
struct Header {
    channels: usize,
    sample_rate: u32,
    max_frame_size: u32,
}

impl Header {
    fn read(extra_data: &[u8]) -> Result<Self> {
        if extra_data.len() < HEADER_SIZE || &extra_data[..4] != b"LSD:" {
            return decode_error("ralf: invalid extra data");
        }

        let be16 = |at: usize| u16::from_be_bytes([extra_data[at], extra_data[at + 1]]);
        let be32 = |at: usize| {
            u32::from_be_bytes([
                extra_data[at],
                extra_data[at + 1],
                extra_data[at + 2],
                extra_data[at + 3],
            ])
        };

        if be16(4) != HEADER_VERSION {
            return unsupported_error("ralf: unsupported version");
        }

        let channels = usize::from(be16(8));
        let sample_rate = be32(12);

        if channels < 1 || channels > 2 || sample_rate < 8000 || sample_rate > 96000 {
            return decode_error("ralf: invalid channel count or sample rate");
        }

        let max_frame_size = be32(16);

        if max_frame_size == 0 || max_frame_size > MAX_FRAME_SIZE_LIMIT {
            return decode_error("ralf: invalid frame size");
        }

        // A frame holds at least one second of audio.
        Ok(Header { channels, sample_rate, max_frame_size: max_frame_size.max(sample_rate) })
    }
}

/// RealAudio Lossless decoder.
pub struct RalfDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    core: Core,
    /// The first half of a split frame, and the room for the continuation of the second.
    pkt: Box<[u8]>,
    /// Whether `pkt` holds the first half of a frame.
    has_pkt: bool,
}

impl RalfDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_RALF {
            return unsupported_error("ralf: invalid codec");
        }

        let Some(extra_data) = &params.extra_data
        else {
            return decode_error("ralf: missing extra data");
        };

        let header = Header::read(extra_data)?;

        let channels = if header.channels == 1 {
            layouts::CHANNEL_LAYOUT_MONO
        }
        else {
            layouts::CHANNEL_LAYOUT_STEREO
        };

        let max_frame_size = header.max_frame_size as usize;

        let buf =
            AudioBuffer::new(AudioSpec::new(header.sample_rate, channels.clone()), max_frame_size);

        let mut params = params.clone();

        params
            .with_sample_rate(header.sample_rate)
            .with_channels(channels)
            .with_sample_format(SampleFormat::S16)
            .with_bits_per_sample(16)
            .with_max_frames_per_packet(u64::from(header.max_frame_size));

        Ok(RalfDecoder {
            params,
            opts: *opts,
            buf,
            core: Core::new(header.channels, max_frame_size)?,
            pkt: vec![0; 2 * MAX_PKT_SIZE].into_boxed_slice(),
            has_pkt: false,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        self.buf.clear();
        self.buf.render_uninit(Some(self.buf.capacity()));

        let (out0, out1): (&mut [i16], &mut [i16]) = if self.core.channels == 2 {
            self.buf.plane_pair_mut(0, 1).ok_or(Error::DecodeError("ralf: missing planes"))?
        }
        else {
            (self.buf.plane_mut(0).ok_or(Error::DecodeError("ralf: missing planes"))?, &mut [])
        };

        let num_samples = if self.has_pkt {
            // The second half of a frame.
            self.has_pkt = false;

            if data.len() < 2 {
                return decode_error("ralf: packet is too short");
            }

            let table_bytes = (usize::from(u16::from_be_bytes([data[0], data[1]])) + 7) >> 3;

            if table_bytes + 3 > data.len() || data.len() > MAX_PKT_SIZE {
                return decode_error("ralf: invalid second half of a split frame");
            }

            // The block table of both halves must match; the rest continues the first.
            if self.pkt[..2 + table_bytes] != data[..2 + table_bytes] {
                return decode_error("ralf: mismatched block table in a split frame");
            }

            let tail = &data[2 + table_bytes..];

            self.pkt[MAX_PKT_SIZE..MAX_PKT_SIZE + tail.len()].copy_from_slice(tail);

            self.core.decode_frame(&self.pkt[..MAX_PKT_SIZE + tail.len()], out0, out1)?
        }
        else if data.len() == MAX_PKT_SIZE {
            // The first half of a frame: wait for the second.
            self.pkt[..MAX_PKT_SIZE].copy_from_slice(data);
            self.has_pkt = true;
            0
        }
        else {
            self.core.decode_frame(data, out0, out1)?
        };

        self.buf.truncate(num_samples);

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for RalfDecoder {
    fn reset(&mut self) {
        self.has_pkt = false;
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

impl RegisterableAudioDecoder for RalfDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(RalfDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_RALF, "ralf", "RealAudio Lossless")]
    }
}
