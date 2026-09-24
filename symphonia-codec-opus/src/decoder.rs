// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The top-level hybrid Opus decoder. Ported from libopus `src/opus_decoder.c`
//! (`OpusDecoder`, `opus_decoder_init`, `opus_decode_native`, `opus_decode_frame`).
//! Ported from libopus (BSD-3-Clause), see NOTICE.

use crate::celt::decoder::CeltDecoder;
use crate::packet::{Bandwidth, OpusMode, Toc};
use crate::silk::{DecControl, DecodeFlag, SilkDecoder};

/// Supported API sample rates, mirroring libopus `opus_decoder_init`'s validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRate {
    Hz8000,
    Hz12000,
    Hz16000,
    Hz24000,
    Hz48000,
}

impl SampleRate {
    pub fn as_hz(self) -> u32 {
        match self {
            SampleRate::Hz8000 => 8000,
            SampleRate::Hz12000 => 12000,
            SampleRate::Hz16000 => 16000,
            SampleRate::Hz24000 => 24000,
            SampleRate::Hz48000 => 48000,
        }
    }
}

/// Decode errors, distinct from [`crate::packet::PacketError`] (which is purely about framing).
#[derive(Debug)]
pub enum DecodeError {
    Packet(crate::packet::PacketError),
    Mapping(crate::mapping::MappingError),
    InvalidChannelCount,
    /// C: `OPUS_BUFFER_TOO_SMALL`.
    BufferTooSmall,
    /// C: `OPUS_BAD_ARG`.
    BadArgument,
    /// C: `OPUS_INTERNAL_ERROR`.
    Internal,
}

impl From<crate::packet::PacketError> for DecodeError {
    fn from(e: crate::packet::PacketError) -> Self {
        DecodeError::Packet(e)
    }
}

impl From<crate::silk::SilkError> for DecodeError {
    fn from(_: crate::silk::SilkError) -> Self {
        DecodeError::Internal
    }
}

pub type Result<T, E = DecodeError> = std::result::Result<T, E>;

/// C: `MODE_SILK_ONLY` / `MODE_HYBRID` / `MODE_CELT_ONLY`, plus libopus's `mode == 0` "no
/// previous frame decoded yet" sentinel represented here as `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Silk,
    Hybrid,
    Celt,
}

impl From<OpusMode> for Mode {
    fn from(m: OpusMode) -> Self {
        match m {
            OpusMode::SilkOnly => Mode::Silk,
            OpusMode::Hybrid => Mode::Hybrid,
            OpusMode::CeltOnly => Mode::Celt,
        }
    }
}

/// C: `smooth_fade`. `other` plays the role of `in1` or `in2` in the crossfade formula, while
/// `out_aliased` plays the other role *and* is the output (in-place, matching every call site in
/// `opus_decode_frame`, which always aliases one input with the output).
enum FadeOther {
    In1,
    In2,
}

fn smooth_fade(
    out_aliased: &mut [f32],
    other: &[f32],
    other_role: FadeOther,
    overlap: usize,
    channels: usize,
    window: &[f32],
    fs: u32,
) {
    let inc = (48000 / fs) as usize;
    for i in 0..overlap {
        let w = window[i * inc] * window[i * inc];
        for c in 0..channels {
            let idx = i * channels + c;
            let aliased_val = out_aliased[idx];
            let other_val = other[idx];
            let (v1, v2) = match other_role {
                FadeOther::In1 => (other_val, aliased_val),
                FadeOther::In2 => (aliased_val, other_val),
            };
            out_aliased[idx] = w * v2 + (1.0 - w) * v1;
        }
    }
}

/// The top-level, single-stream Opus decoder. C: `struct OpusDecoder`.
pub struct OpusDecoder {
    channels: u8,
    sample_rate: SampleRate,
    silk: SilkDecoder,
    celt: CeltDecoder,
    /// C: `st->DecControl`. NOT cleared on reset (declared before `OPUS_DECODER_RESET_START`).
    dec_control: DecControl,
    /// C: `st->decode_gain`, Q8 dB units (`OPUS_SET_GAIN`). NOT cleared on reset.
    decode_gain: i16,

    // Fields below mirror `OPUS_DECODER_RESET_START` onward in the C struct: cleared on reset.
    stream_channels: u8,
    bandwidth: Option<Bandwidth>,
    mode: Mode,
    prev_mode: Mode,
    frame_size: usize,
    prev_redundancy: bool,
    last_packet_duration: usize,
    range_final: u32,
}

impl OpusDecoder {
    /// C: `opus_decoder_init` (combined with allocation, unlike the C two-step
    /// `opus_decoder_create`/`opus_decoder_init` split which exists only for fixed-size
    /// allocation reasons that don't apply to a Rust `Vec`/`Box`-based port).
    pub fn try_new(sample_rate: SampleRate, channels: u8) -> Result<Self> {
        if channels != 1 && channels != 2 {
            return Err(DecodeError::InvalidChannelCount);
        }
        let fs = sample_rate.as_hz();
        Ok(OpusDecoder {
            channels,
            sample_rate,
            silk: SilkDecoder::new(),
            celt: CeltDecoder::new(fs, channels),
            dec_control: DecControl {
                n_channels_api: channels as i32,
                api_sample_rate: fs as i32,
                ..Default::default()
            },
            decode_gain: 0,
            stream_channels: channels,
            bandwidth: None,
            mode: Mode::None,
            prev_mode: Mode::None,
            frame_size: fs as usize / 400,
            prev_redundancy: false,
            last_packet_duration: 0,
            range_final: 0,
        })
    }

    /// C: the `OPUS_RESET_STATE` CTL path (clears fields from `OPUS_DECODER_RESET_START` on).
    pub fn reset(&mut self) {
        self.silk.reset();
        self.celt.reset();
        self.stream_channels = self.channels;
        self.bandwidth = None;
        self.mode = Mode::None;
        self.prev_mode = Mode::None;
        self.frame_size = self.sample_rate.as_hz() as usize / 400;
        self.prev_redundancy = false;
        self.last_packet_duration = 0;
        self.range_final = 0;
    }

    /// C: `opus_decoder_get_nb_samples` / final range accessor used by
    /// `OPUS_GET_FINAL_RANGE`. Exposed for the RFC 8251 conformance harness.
    pub fn final_range(&self) -> u32 {
        self.range_final
    }

    /// C: `OPUS_GET_LAST_PACKET_DURATION_REQUEST`.
    pub fn last_packet_duration(&self) -> usize {
        self.last_packet_duration
    }

    /// C: `OPUS_SET_GAIN_REQUEST`. `gain` is in Q8 dB units, matching RFC 7845's `OpusHead`
    /// `output_gain` field directly (both are `1/256th dB`).
    pub fn set_gain(&mut self, gain: i16) {
        self.decode_gain = gain;
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// C: `opus_decode_float` / `opus_decode_native` with `decode_fec = 0`,
    /// `self_delimited = 0`. The single-packet decode entry point.
    ///
    /// `data == None` requests PLC for `frame_size` samples (C: `data == NULL` path).
    pub fn decode(&mut self, data: Option<&[u8]>, out: &mut [f32], frame_size: usize) -> Result<usize> {
        self.decode_native(data, out, frame_size, false, false).map(|(n, _)| n)
    }

    /// C: `opus_decode_native`. Returns `(samples_decoded, packet_offset)`; `packet_offset` is
    /// the number of bytes consumed from `data` when `self_delimited` is set (used by
    /// [`crate::multistream::MultistreamDecoder`] to split a multistream packet).
    pub fn decode_native(
        &mut self,
        data: Option<&[u8]>,
        out: &mut [f32],
        frame_size: usize,
        decode_fec: bool,
        self_delimited: bool,
    ) -> Result<(usize, usize)> {
        let ch = self.channels as usize;
        let f2_5 = self.sample_rate.as_hz() as usize / 400;

        if (decode_fec || data.map_or(true, |d| d.is_empty())) && frame_size % f2_5 != 0 {
            return Err(DecodeError::BadArgument);
        }

        if data.map_or(true, |d| d.is_empty()) {
            let mut pcm_count = 0usize;
            loop {
                let ret = self.decode_frame(None, &mut out[pcm_count * ch..], frame_size - pcm_count, false)?;
                pcm_count += ret;
                if pcm_count >= frame_size {
                    break;
                }
            }
            self.last_packet_duration = pcm_count;
            return Ok((pcm_count, 0));
        }
        let data = data.unwrap();

        let toc = Toc::new(data[0]);
        let packet_mode = toc.mode();
        let packet_bandwidth = toc.bandwidth();
        let packet_frame_size = toc.samples_per_frame(self.sample_rate.as_hz()) as usize;
        let packet_stream_channels = if toc.stereo() { 2 } else { 1 };

        let parsed = crate::packet::parse_impl(data, self_delimited)?;
        let count = parsed.frames.len();
        let packet_offset = parsed.packet_len;

        if decode_fec {
            if frame_size < packet_frame_size || packet_mode == OpusMode::CeltOnly || self.mode == Mode::Celt {
                return self.decode_native(None, out, frame_size, false, false);
            }
            let duration_copy = self.last_packet_duration;
            if frame_size != packet_frame_size {
                if let Err(e) = self.decode_native(None, out, frame_size - packet_frame_size, false, false) {
                    self.last_packet_duration = duration_copy;
                    return Err(e);
                }
            }
            self.mode = packet_mode.into();
            self.bandwidth = Some(packet_bandwidth);
            self.frame_size = packet_frame_size;
            self.stream_channels = packet_stream_channels;
            let fr = parsed.frames[0];
            let frame_data = &data[fr.offset..fr.offset + fr.len];
            self.decode_frame(
                Some(frame_data),
                &mut out[ch * (frame_size - packet_frame_size)..],
                packet_frame_size,
                true,
            )?;
            self.last_packet_duration = frame_size;
            return Ok((frame_size, packet_offset));
        }

        if count * packet_frame_size > frame_size {
            return Err(DecodeError::BufferTooSmall);
        }

        self.mode = packet_mode.into();
        self.bandwidth = Some(packet_bandwidth);
        self.frame_size = packet_frame_size;
        self.stream_channels = packet_stream_channels;

        let mut nb_samples = 0usize;
        for fr in &parsed.frames {
            let frame_data = &data[fr.offset..fr.offset + fr.len];
            let ret = self.decode_frame(Some(frame_data), &mut out[nb_samples * ch..], frame_size - nb_samples, false)?;
            nb_samples += ret;
        }
        self.last_packet_duration = nb_samples;
        Ok((nb_samples, packet_offset))
    }

    /// C: `opus_decode_frame`. Decodes a single TOC-config frame (one SILK/Hybrid/CELT unit,
    /// possibly with embedded redundancy) or runs PLC (`data == None`).
    fn decode_frame<'a>(
        &mut self,
        data: Option<&'a [u8]>,
        pcm: &mut [f32],
        frame_size_in: usize,
        decode_fec: bool,
    ) -> Result<usize> {
        let fs = self.sample_rate.as_hz() as usize;
        let ch = self.channels as usize;
        let f20 = fs / 50;
        let f10 = f20 / 2;
        let f5 = f10 / 2;
        let f2_5 = f5 / 2;

        if frame_size_in < f2_5 {
            return Err(DecodeError::BufferTooSmall);
        }
        let mut frame_size = frame_size_in.min(fs / 25 * 3);

        // Payloads of 1 (2 including ToC) or 0 trigger the PLC/DTX.
        let mut data = data;
        if let Some(d) = data {
            if d.len() <= 1 {
                data = None;
                frame_size = frame_size.min(self.frame_size);
            }
        }

        let mut audiosize: usize;
        let mode: Mode;
        let bandwidth: Option<Bandwidth>;

        if data.is_some() {
            audiosize = self.frame_size;
            mode = self.mode;
            bandwidth = self.bandwidth;
        }
        else {
            audiosize = frame_size;
            mode = if self.prev_redundancy { Mode::Celt } else { self.prev_mode };
            bandwidth = None;

            if mode == Mode::None {
                for v in pcm[..audiosize * ch].iter_mut() {
                    *v = 0.0;
                }
                return Ok(audiosize);
            }

            if audiosize > f20 {
                let mut remaining = audiosize;
                let mut produced = 0usize;
                loop {
                    let chunk = remaining.min(f20);
                    let ret = self.decode_frame(None, &mut pcm[produced * ch..], chunk, false)?;
                    produced += ret;
                    remaining -= ret;
                    if remaining == 0 {
                        break;
                    }
                }
                return Ok(frame_size);
            }
            else if audiosize < f20 {
                if audiosize > f10 {
                    audiosize = f10;
                }
                else if mode != Mode::Silk && audiosize > f5 && audiosize < f10 {
                    audiosize = f5;
                }
            }
        }

        let original_storage = data.map(|d| d.len() as i64).unwrap_or(0);
        let mut dec = crate::range::RangeDecoder::new(data.unwrap_or(&[]));
        let mut len: i64 = original_storage;

        // celt_accum is always false: only the FIXED_POINT build sets it (a stack-space
        // optimization that accumulates CELT output directly on top of the SILK PCM buffer,
        // which doesn't apply to this float-only port).

        let transition_early = data.is_some()
            && self.prev_mode != Mode::None
            && ((mode == Mode::Celt && self.prev_mode != Mode::Celt && !self.prev_redundancy)
                || (mode != Mode::Celt && self.prev_mode == Mode::Celt));
        let mut transition = transition_early;

        let mut pcm_transition: Vec<f32> = Vec::new();
        if transition && mode == Mode::Celt {
            pcm_transition = vec![0f32; f5 * ch];
            let chunk = f5.min(audiosize);
            let _ = self.decode_frame(None, &mut pcm_transition, chunk, false);
        }

        if audiosize > frame_size {
            return Err(DecodeError::BadArgument);
        }
        let frame_size = audiosize;

        // SILK processing: decodes into a separate int16 buffer, mixed into `pcm` *after* CELT
        // (which, for Hybrid, overwrites `pcm` directly via deemphasis -- SILK's contribution
        // must not be clobbered by that).
        let mut pcm_silk: Vec<i16> = Vec::new();
        if mode != Mode::Celt {
            if self.prev_mode == Mode::Celt {
                self.silk.reset();
            }

            self.dec_control.payload_size_ms = ((1000 * audiosize / fs).max(10)) as i32;

            if data.is_some() {
                self.dec_control.n_channels_internal = self.stream_channels as i32;
                self.dec_control.internal_sample_rate = if mode == Mode::Silk {
                    match bandwidth {
                        Some(Bandwidth::Narrowband) => 8000,
                        Some(Bandwidth::Mediumband) => 12000,
                        _ => 16000,
                    }
                }
                else {
                    16000
                };
            }

            let lost_flag = if data.is_none() {
                DecodeFlag::PacketLost
            }
            else if decode_fec {
                DecodeFlag::Lbrr
            }
            else {
                DecodeFlag::Normal
            };

            pcm_silk = vec![0i16; frame_size.max(f10) * ch];
            let mut decoded_samples = 0usize;
            loop {
                let first_frame = decoded_samples == 0;
                let mut silk_frame_size = 0usize;
                let res = self.silk.decode(
                    &mut self.dec_control,
                    lost_flag,
                    first_frame,
                    &mut dec,
                    &mut pcm_silk[decoded_samples * ch..],
                    &mut silk_frame_size,
                );
                if res.is_err() {
                    if lost_flag != DecodeFlag::Normal {
                        silk_frame_size = frame_size - decoded_samples;
                        for v in pcm_silk[decoded_samples * ch..(decoded_samples + silk_frame_size) * ch].iter_mut() {
                            *v = 0;
                        }
                    }
                    else {
                        return Err(DecodeError::Internal);
                    }
                }
                decoded_samples += silk_frame_size;
                if decoded_samples >= frame_size {
                    break;
                }
            }
        }

        let mut start_band = 0i32;
        let mut redundancy = false;
        let mut celt_to_silk = false;
        let mut redundancy_bytes: i64 = 0;

        if !decode_fec && mode != Mode::Celt && data.is_some() {
            let extra: i64 = 17 + if mode == Mode::Hybrid { 20 } else { 0 };
            if (dec.tell() as i64) + extra <= 8 * len {
                redundancy = if mode == Mode::Hybrid { dec.dec_bit_logp(12) } else { true };
                if redundancy {
                    celt_to_silk = dec.dec_bit_logp(1);
                    redundancy_bytes = if mode == Mode::Hybrid {
                        dec.dec_uint(256) as i64 + 2
                    }
                    else {
                        len - (((dec.tell() as i64) + 7) >> 3)
                    };
                    len -= redundancy_bytes;
                    if len * 8 < dec.tell() as i64 {
                        len = 0;
                        redundancy_bytes = 0;
                        redundancy = false;
                    }
                    dec.shrink_storage((original_storage - redundancy_bytes) as u32);
                }
            }
        }
        if mode != Mode::Celt {
            start_band = 17;
        }

        if redundancy {
            transition = false;
        }

        if transition && mode != Mode::Celt {
            pcm_transition = vec![0f32; f5 * ch];
            let chunk = f5.min(audiosize);
            let _ = self.decode_frame(None, &mut pcm_transition, chunk, false);
        }

        if let Some(bw) = bandwidth {
            let endband = match bw {
                Bandwidth::Narrowband => 13,
                Bandwidth::Mediumband | Bandwidth::Wideband => 17,
                Bandwidth::Superwideband => 19,
                Bandwidth::Fullband => 21,
            };
            self.celt.set_end_band(endband);
        }
        self.celt.set_channels(self.stream_channels as i32);

        let mut redundant_audio: Vec<f32> = if redundancy { vec![0f32; f5 * ch] } else { Vec::new() };
        let mut redundant_rng: u32 = 0;

        // 5 ms redundant frame for CELT->SILK.
        if redundancy && celt_to_silk {
            self.celt.set_start_band(0);
            let start = len as usize;
            let redund = &data.unwrap()[start..start + redundancy_bytes as usize];
            let _ = self.celt.decode_with_ec(Some(redund), &mut redundant_audio, f5 as i32, None, false);
            redundant_rng = self.celt.final_range();
        }

        // MUST be after PLC.
        self.celt.set_start_band(start_band);

        let mut celt_ret: std::result::Result<usize, i32> = Ok(0);
        if mode != Mode::Silk {
            let celt_frame_size = f20.min(frame_size);
            if mode != self.prev_mode && self.prev_mode != Mode::None && !self.prev_redundancy {
                self.celt.reset();
            }
            // C passes `len`, which excludes any trailing redundant frame; CELT derives its bit
            // budget from it (`total_bits = len*8`).
            let celt_data = if decode_fec { None } else { data.map(|d| &d[..len as usize]) };
            celt_ret = self.celt.decode_with_ec(celt_data, pcm, celt_frame_size as i32, Some(&mut dec), false);
        }
        else {
            for v in pcm[..frame_size * ch].iter_mut() {
                *v = 0.0;
            }
            if self.prev_mode == Mode::Hybrid && !(redundancy && celt_to_silk && self.prev_redundancy) {
                self.celt.set_start_band(0);
                let silence = [0xFFu8, 0xFFu8];
                let _ = self.celt.decode_with_ec(Some(&silence), pcm, f2_5 as i32, None, false);
            }
        }

        // Mix SILK's int16 PCM into the (float) output buffer. C: `if (mode != MODE_CELT_ONLY
        // && !celt_accum)`, always taken here (`celt_accum` is FIXED_POINT-only).
        if mode != Mode::Celt {
            for i in 0..frame_size * ch {
                pcm[i] += pcm_silk[i] as f32 * (1.0 / 32768.0);
            }
        }

        let window = crate::celt::modes::MODE_48000_960.window;
        let fs_u32 = self.sample_rate.as_hz();

        // 5 ms redundant frame for SILK->CELT.
        if redundancy && !celt_to_silk {
            self.celt.reset();
            self.celt.set_start_band(0);
            let start = len as usize;
            let redund = &data.unwrap()[start..start + redundancy_bytes as usize];
            let _ = self.celt.decode_with_ec(Some(redund), &mut redundant_audio, f5 as i32, None, false);
            redundant_rng = self.celt.final_range();
            let off = ch * (frame_size - f2_5);
            let roff = ch * f2_5;
            smooth_fade(&mut pcm[off..off + ch * f2_5], &redundant_audio[roff..roff + ch * f2_5], FadeOther::In2, f2_5, ch, window, fs_u32);
        }
        // 5 ms redundant frame for CELT->SILK; ignore if the previous frame did not use CELT.
        if redundancy && celt_to_silk && (self.prev_mode != Mode::Silk || self.prev_redundancy) {
            for c in 0..ch {
                for i in 0..f2_5 {
                    pcm[ch * i + c] = redundant_audio[ch * i + c];
                }
            }
            let off = ch * f2_5;
            smooth_fade(&mut pcm[off..off + ch * f2_5], &redundant_audio[off..off + ch * f2_5], FadeOther::In1, f2_5, ch, window, fs_u32);
        }
        if transition {
            if audiosize >= f5 {
                pcm[..ch * f2_5].copy_from_slice(&pcm_transition[..ch * f2_5]);
                let off = ch * f2_5;
                smooth_fade(&mut pcm[off..off + ch * f2_5], &pcm_transition[off..off + ch * f2_5], FadeOther::In1, f2_5, ch, window, fs_u32);
            }
            else {
                smooth_fade(&mut pcm[..ch * f2_5], &pcm_transition[..ch * f2_5], FadeOther::In1, f2_5, ch, window, fs_u32);
            }
        }

        if self.decode_gain != 0 {
            let gain = 2f32.powf(6.48814081e-4 * self.decode_gain as f32);
            for v in pcm[..frame_size * ch].iter_mut() {
                *v *= gain;
            }
        }

        self.range_final = if len <= 1 { 0 } else { dec.range() ^ redundant_rng };

        self.prev_mode = mode;
        self.prev_redundancy = redundancy && !celt_to_silk;

        match celt_ret {
            Ok(_) => Ok(audiosize),
            Err(_) => Err(DecodeError::Internal),
        }
    }
}
