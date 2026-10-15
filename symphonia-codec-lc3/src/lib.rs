// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
// The tables are literals of the reference, some of which are close to a constant of the standard
// library.
#![allow(clippy::approx_constant)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// The decoder is a function-by-function port of the reference implementation, which indexes
// several arrays with one counter.
#![allow(clippy::needless_range_loop)]

//! A pure Rust decoder of LC3 (Low Complexity Communication Codec, ETSI TS 103 634) and LC3plus,
//! including the high resolution modes.
//!
//! The decoder is a port of the decoder of the `liblc3` library of Google, which is the
//! reference for its output.
//!
//! # Codec parameters
//!
//! The sample rate and the channels are taken from the codec parameters, and the frame duration
//! and the modes from the 6 bytes of `extra_data`, which are three little-endian 16-bit
//! integers: the frame duration in units of 10 µs, the error protection mode (only 0, no error
//! protection, is supported), and the high resolution mode flag. This is the layout of the
//! `extradata` of FFmpeg and of the fields of the header of `.lc3` files.
//!
//! A packet holds one frame of every channel. The frames of the channels have the same size,
//! except that the first `size % channels` channels have one byte more. An empty packet is
//! concealed as a lost frame, and so is a frame with an invalid bitstream. Only the decoding to
//! the sample rate of the stream is supported.

mod bits;
mod common;
mod decoder;
mod fastmath;
mod ltpf;
mod mdct;
mod sns;
mod spec;
mod tables;
mod tns;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_LC3;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use decoder::{FrameDecoder, Mode};

/// The size of the `extra_data`.
const EXTRA_DATA_SIZE: usize = 6;
/// The most channels that are accepted (`LC3_MAX_CHANNELS` of the tools of the reference).
const MAX_CHANNELS: usize = 8;

/// Returns the algorithmic delay of a stream, as a number of samples per channel
/// (`lc3_hr_delay_samples`), or `None` if the parameters are not valid.
pub fn delay_samples(frame_duration_us: u32, sample_rate: u32, hr_mode: bool) -> Option<u32> {
    let dt = common::resolve_dt(frame_duration_us, hr_mode)?;
    let sr = common::resolve_srate(sample_rate, hr_mode)?;

    Some((2 * common::nd(dt, sr) - common::ns(dt, sr)) as u32)
}

/// Returns the number of samples per channel of a frame, or `None` if the parameters are not
/// valid.
pub fn frame_samples(frame_duration_us: u32, sample_rate: u32, hr_mode: bool) -> Option<u32> {
    let dt = common::resolve_dt(frame_duration_us, hr_mode)?;
    let sr = common::resolve_srate(sample_rate, hr_mode)?;

    Some(common::ns(dt, sr) as u32)
}

/// LC3 / LC3plus decoder.
pub struct Lc3Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    ns: usize,
    num_channels: usize,
    core: FrameDecoder,
    buf: AudioBuffer<f32>,
}

impl Lc3Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        // Verify codec ID.
        if params.codec != CODEC_ID_LC3 {
            return unsupported_error("lc3: invalid codec");
        }

        let extra_data = match &params.extra_data {
            Some(extra_data) if extra_data.len() >= EXTRA_DATA_SIZE => extra_data,
            _ => return decode_error("lc3: missing or short extra data"),
        };

        let u16_at =
            |offset: usize| u16::from_le_bytes([extra_data[offset], extra_data[offset + 1]]);

        let frame_us = u32::from(u16_at(0)) * 10;
        let ep_mode = u16_at(2);
        let hr_mode = u16_at(4) != 0;

        if ep_mode != 0 {
            return unsupported_error("lc3: error protection is not supported");
        }

        let Some(sample_rate) = params.sample_rate
        else {
            return unsupported_error("lc3: unknown sample rate");
        };

        let num_channels = params.channels.as_ref().map_or(0, Channels::count);
        if num_channels == 0 || num_channels > MAX_CHANNELS {
            return unsupported_error("lc3: unsupported number of channels");
        }

        let Some(dt) = common::resolve_dt(frame_us, hr_mode)
        else {
            return unsupported_error("lc3: unsupported frame duration");
        };
        let Some(sr) = common::resolve_srate(sample_rate, hr_mode)
        else {
            return unsupported_error("lc3: unsupported sample rate");
        };

        let mode = Mode { dt, sr };
        let ns = mode.ns();

        let channels = match &params.channels {
            Some(channels) if num_channels > 2 => channels.clone(),
            _ if num_channels == 1 => Channels::Positioned(Position::FRONT_LEFT),
            _ => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
        };

        let buf = AudioBuffer::new(AudioSpec::new(sample_rate, channels.clone()), ns);

        let mut params = params.clone();
        params.with_sample_rate(sample_rate);
        params.with_channels(channels);

        Ok(Lc3Decoder {
            params,
            opts: *opts,
            ns,
            num_channels,
            core: FrameDecoder::new(mode, num_channels),
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;
        let nch = self.num_channels;

        self.buf.clear();

        // The frames of the channels share the size of the packet. An empty packet stands for a
        // lost one.
        let size_of = |ch: usize| data.len() / nch + usize::from(ch < data.len() % nch);

        if !data.is_empty() {
            let max = self.core.max_frame_bytes();

            for ch in 0..nch {
                let nbytes = size_of(ch);

                if nbytes < common::MIN_FRAME_BYTES || nbytes > max {
                    return decode_error("lc3: invalid frame size");
                }
            }
        }

        self.buf.resize_uninit(self.ns);

        let mut offset = 0;

        for ch in 0..nch {
            let frame = if data.is_empty() {
                None
            }
            else {
                let nbytes = size_of(ch);
                let frame = &data[offset..offset + nbytes];
                offset += nbytes;
                Some(frame)
            };

            let Some(plane) = self.buf.plane_mut(ch)
            else {
                self.buf.clear();
                return decode_error("lc3: missing plane");
            };

            if self.core.decode(ch, frame, plane) {
                log::debug!("lc3: invalid frame, concealed");
            }
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for Lc3Decoder {
    fn reset(&mut self) {
        self.core.reset();
    }

    fn codec_info(&self) -> &CodecInfo {
        // Only one codec is supported.
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

impl RegisterableAudioDecoder for Lc3Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Lc3Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_LC3, "lc3", "LC3 / LC3plus")]
    }
}
