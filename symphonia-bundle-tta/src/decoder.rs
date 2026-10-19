// Symphonia TTA Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The TTA1 decoder.
//!
//! Follows FFmpeg `libavcodec/tta.c` (`tta_decode_frame`), `ttadsp.c` (`tta_filter_process_c`) and
//! `ttadata.c`, and is bit-exact with it, including the frames it rejects. All the arithmetic of
//! the reference wraps around on overflow, which only a damaged stream can cause.
//!
//! A frame holds 256/245 s of audio (the last one of a file less). Each channel of a frame is
//! coded independently with
//!
//! 1. an adaptive Rice code, with two parameter sets selected by the unary prefix of a value,
//! 2. a hybrid filter: an 8-tap sign-sign LMS filter,
//! 3. a fixed first-order prediction,
//!
//! and the channels are then decorrelated: the last channel is the "mid" of the previous one, and
//! the other channels are coded as the difference to their successor.

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TTA;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::bits::BitReaderLe;
use crate::header::{
    FORMAT_ENCRYPTED, FORMAT_SIMPLE, HEADER_LEN, Header, MAX_BITS_PER_SAMPLE, MAX_CHANNELS,
    channels_from_count, check_params, crc32, frame_length,
};

/// The Rice parameter thresholds `ff_tta_shift_1`: `2^i` up to `2^31`, saturated to `2^31` and
/// ended by `u32::MAX`.
static SHIFT_1: [u32; 41] = {
    let mut t = [0x8000_0000u32; 41];
    let mut i = 0;
    while i < 32 {
        t[i] = 1 << i;
        i += 1;
    }
    t[40] = u32::MAX;
    t
};

/// The threshold of the Rice parameter `k` (`ff_tta_shift_16[k]`, which is `ff_tta_shift_1 + 4`).
///
/// `k` is at most 35: it is only incremented if the sum is greater than `shift_16(k + 1)`, which
/// is `u32::MAX` for `k = 35`.
#[inline]
fn shift_16(k: u32) -> u32 {
    SHIFT_1[k as usize + 4]
}

/// The number of bits `s->bps` of the filter configuration table `ff_tta_filter_configs`
/// (indexed by the number of bytes per sample, minus 1): the shift of the output of the filter.
const FILTER_SHIFTS: [u32; 3] = [10, 9, 10];

/// The most bits that FFmpeg's `get_bits` reads at once (`MIN_CACHE_BITS`). A Rice code with a
/// greater parameter is rejected as invalid data.
const MAX_RICE_BITS: u32 = 25;

/// The hybrid filter (`TTAFilter`, and `tta_filter_process_c`).
#[derive(Copy, Clone, Default)]
struct Filter {
    qm: [i32; 8],
    dx: [i32; 8],
    dl: [i32; 8],
    error: i32,
    shift: u32,
    round: i32,
}

impl Filter {
    /// `ff_tta_filter_init`.
    fn new(shift: u32) -> Self {
        Filter { shift, round: SHIFT_1[shift as usize - 1] as i32, ..Default::default() }
    }

    /// Filters a value, and adapts the filter.
    #[inline]
    fn process(&mut self, input: i32) -> i32 {
        let Filter { qm, dx, dl, error, shift, round } = self;

        if *error < 0 {
            for (q, d) in qm.iter_mut().zip(dx.iter()) {
                *q = q.wrapping_sub(*d);
            }
        }
        else if *error > 0 {
            for (q, d) in qm.iter_mut().zip(dx.iter()) {
                *q = q.wrapping_add(*d);
            }
        }

        let mut sum = *round;
        for (l, q) in dl.iter().zip(qm.iter()) {
            sum = sum.wrapping_add(l.wrapping_mul(*q));
        }

        dx.copy_within(1..5, 0);
        dl.copy_within(1..5, 0);

        dx[4] = (dl[4] >> 30) | 1;
        dx[5] = ((dl[5] >> 30) | 2) & !1;
        dx[6] = ((dl[6] >> 30) | 2) & !1;
        dx[7] = ((dl[7] >> 30) | 4) & !3;

        *error = input;
        let out = input.wrapping_add(sum >> *shift);

        dl[4] = 0i32.wrapping_sub(dl[5]);
        dl[5] = 0i32.wrapping_sub(dl[6]);
        dl[6] = out.wrapping_sub(dl[7]);
        dl[7] = out;
        dl[5] = dl[5].wrapping_add(dl[6]);
        dl[4] = dl[4].wrapping_add(dl[5]);

        out
    }
}

/// The state of the decoder of one channel of a frame (`TTAChannel`).
#[derive(Copy, Clone)]
struct Channel {
    filter: Filter,
    predictor: i32,
    k0: u32,
    k1: u32,
    sum0: u32,
    sum1: u32,
}

impl Channel {
    /// The state at the start of every frame.
    fn new(filter_shift: u32) -> Self {
        Channel {
            filter: Filter::new(filter_shift),
            predictor: 0,
            k0: 10,
            k1: 10,
            sum0: shift_16(10),
            sum1: shift_16(10),
        }
    }

    /// Decodes the next sample of the channel. `pred_shift` is the shift of the fixed order
    /// predictor.
    #[inline]
    fn decode(&mut self, br: &mut BitReaderLe<'_>, pred_shift: u32) -> Result<i32> {
        let mut unary = br.read_unary_ones();

        let (depth, k) = if unary == 0 {
            (0, self.k0)
        }
        else {
            unary -= 1;
            (1, self.k1)
        };

        if br.bits_left() < k as usize {
            return decode_error("tta: unexpected end of frame");
        }

        let mut value = if k != 0 {
            if k > MAX_RICE_BITS || unary > (i32::MAX as u64) >> k {
                return decode_error("tta: invalid rice code");
            }
            (((unary as u32) << k) + br.read_bits(k)) as i32
        }
        else if unary > i32::MAX as u64 {
            return decode_error("tta: invalid rice code");
        }
        else {
            unary as i32
        };

        if depth == 1 {
            self.sum1 = self.sum1.wrapping_add(value as u32).wrapping_sub(self.sum1 >> 4);
            if self.k1 > 0 && self.sum1 < shift_16(self.k1) {
                self.k1 -= 1;
            }
            else if self.sum1 > shift_16(self.k1 + 1) {
                self.k1 += 1;
            }
            value = value.wrapping_add(SHIFT_1[self.k0 as usize] as i32);
        }

        self.sum0 = self.sum0.wrapping_add(value as u32).wrapping_sub(self.sum0 >> 4);
        if self.k0 > 0 && self.sum0 < shift_16(self.k0) {
            self.k0 -= 1;
        }
        else if self.sum0 > shift_16(self.k0 + 1) {
            self.k0 += 1;
        }

        // Extract the coded value: the zig-zag decoded value, plus one.
        let value = 1i32.wrapping_add((value >> 1) ^ ((value & 1) - 1));

        let value = self.filter.process(value);

        // The fixed order predictor: `x - (x >> k)` computed with 64-bit intermediates (`PRED`).
        let p = self.predictor as i64 as u64;
        let pred = (((p << pred_shift).wrapping_sub(p)) >> pred_shift) as i32;

        let value = value.wrapping_add(pred);
        self.predictor = value;

        Ok(value)
    }
}

/// True Audio (TTA1) decoder.
///
/// Each packet must hold a whole frame, with its CRC, as stored in `.tta` files and Matroska.
pub struct TtaDecoder {
    params: AudioCodecParameters,
    buf: AudioBuffer<i32>,
    /// The state of the channels at the start of a frame.
    initial: Vec<Channel>,
    /// The state of the channels.
    chans: Vec<Channel>,
    /// The shift of the fixed order predictor.
    pred_shift: u32,
    /// The shift that left-justifies a sample in 32 bits.
    out_shift: u32,
    /// The number of samples per channel of a full frame.
    frame_length: usize,
    /// The number of samples of the last frame as the header gives it, or 0 if unknown or if all
    /// the frames are full.
    last_frame_length: usize,
    verify: bool,
}

impl TtaDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_TTA {
            return unsupported_error("tta: invalid codec");
        }

        // The header of the file, which `.tta` files and `A_TTA1` tracks written by FFmpeg that
        // the reader turns into codec parameters have, or else the parameters of the track alone.
        let header = match &params.extra_data {
            Some(extra) => {
                let bytes = extra
                    .get(..HEADER_LEN)
                    .and_then(|b| <&[u8; HEADER_LEN]>::try_from(b).ok())
                    .ok_or(Error::DecodeError("tta: invalid extra data"))?;
                Header::parse(bytes)?
            }
            None => {
                let channels =
                    params.channels.as_ref().ok_or(Error::DecodeError("tta: missing channels"))?;
                Header {
                    format: FORMAT_SIMPLE,
                    channels: u16::try_from(channels.count())
                        .map_err(|_| Error::DecodeError("tta: invalid number of channels"))?,
                    bits_per_sample: params
                        .bits_per_sample
                        .and_then(|b| u16::try_from(b).ok())
                        .ok_or(Error::DecodeError("tta: missing bits per sample"))?,
                    sample_rate: params
                        .sample_rate
                        .ok_or(Error::DecodeError("tta: missing sample rate"))?,
                    data_length: 0,
                }
            }
        };

        if header.format == FORMAT_ENCRYPTED {
            return unsupported_error("tta: encrypted streams are not supported");
        }
        else if header.format > FORMAT_ENCRYPTED {
            return decode_error("tta: invalid format");
        }

        check_params(header.channels, header.sample_rate)?;

        let bps = header.bytes_per_sample();
        if header.bits_per_sample == 0 || header.bits_per_sample > MAX_BITS_PER_SAMPLE {
            return unsupported_error("tta: unsupported bit depth");
        }

        let channels = usize::from(header.channels);
        debug_assert!(channels <= usize::from(MAX_CHANNELS));

        let frame_len = frame_length(header.sample_rate) as usize;
        let last_frame_length = (u64::from(header.data_length) % frame_len as u64) as usize;

        let layout = channels_from_count(header.channels);
        let buf = AudioBuffer::new(AudioSpec::new(header.sample_rate, layout.clone()), frame_len);

        let initial = vec![Channel::new(FILTER_SHIFTS[bps - 1]); channels];

        let mut params = params.clone();
        params
            .with_sample_rate(header.sample_rate)
            .with_channels(layout)
            .with_bits_per_sample(u32::from(header.bits_per_sample));

        Ok(TtaDecoder {
            params,
            buf,
            chans: initial.clone(),
            initial,
            pred_shift: if bps == 1 { 4 } else { 5 },
            out_shift: 32 - 8 * bps as u32,
            frame_length: frame_len,
            last_frame_length,
            verify: opts.verify,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        if self.verify {
            let Some(body_len) = data.len().checked_sub(4)
            else {
                return decode_error("tta: packet too short");
            };
            let (body, crc) = data.split_at(body_len);
            if crc32(body) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
                return decode_error("tta: frame crc mismatch");
            }
        }

        // The duration of the packet, if the demuxer tells and it is that of a short frame, is a
        // second candidate for the length of the last frame.
        let packet_dur = usize::try_from(packet.dur.get()).unwrap_or(0);
        let packet_dur = if packet_dur < self.frame_length { packet_dur } else { 0 };

        self.buf.clear();
        self.buf.render_uninit(Some(self.frame_length));

        self.chans.copy_from_slice(&self.initial);

        let channels = self.chans.len();
        let mut br = BitReaderLe::new(data);
        let mut frames = self.frame_length;
        let mut samples = [0i32; MAX_CHANNELS as usize];
        let samples = &mut samples[..channels];

        for t in 0..self.frame_length {
            // A frame of which the data ended can only fail, see below.
            if br.bits_left() == 0 {
                return decode_error("tta: unexpected end of frame");
            }

            for (sample, chan) in samples.iter_mut().zip(self.chans.iter_mut()) {
                *sample = chan.decode(&mut br, self.pred_shift)?;
            }

            // Decorrelate the channels.
            if channels > 1 {
                let last = channels - 1;
                samples[last] = samples[last].wrapping_add(samples[last - 1] / 2);
                for r in (0..last).rev() {
                    samples[r] = samples[r + 1].wrapping_sub(samples[r]);
                }
            }

            for (c, sample) in samples.iter().enumerate() {
                if let Some(plane) = self.buf.plane_mut(c) {
                    plane[t] = *sample << self.out_shift;
                }
            }

            // A frame that is shorter than the others, the last of a stream, is cut where the
            // data of the samples ends, and the CRC (and the padding to a byte) is left.
            let done = t + 1;
            if (done == self.last_frame_length || done == packet_dur) && br.bits_left() / 8 == 4 {
                frames = done;
                break;
            }
        }

        self.buf.truncate(frames);

        // The CRC follows the data, at a byte boundary.
        br.align();
        if !br.skip(32) {
            return decode_error("tta: unexpected end of frame");
        }

        Ok(())
    }
}

impl AudioDecoder for TtaDecoder {
    fn reset(&mut self) {
        // The frames are independent of each other.
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

impl RegisterableAudioDecoder for TtaDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(TtaDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_TTA, "tta", "True Audio (TTA)")]
    }
}
