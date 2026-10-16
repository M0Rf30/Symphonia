// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC1 and ATRAC3 decoders.
//!
//! ATRAC (Adaptive TRansform Acoustic Coding) is the transform codec of Sony's MiniDisc. There is
//! no public specification; these decoders follow the structure of the FFmpeg `atrac1` and
//! `atrac3` decoders (see `NOTICE`).
//!
//! # ATRAC1
//!
//! A packet holds one or more whole frames of `212 * channels` bytes (one 212-byte sound unit per
//! channel, e.g. as stored in AEA files), each of which decodes to 512 samples per channel. No
//! extra data is needed.
//!
//! # ATRAC3
//!
//! A packet holds exactly one frame of `block_align` bytes, which decodes to 1024 samples per
//! channel. The codec extra data is required and may have either layout used by the containers:
//!
//! * 14 bytes: the `fmt ` chunk extension of WAVE files (format tag 0x0270). The frame size must
//!   be `{96, 152, 192} * channels * frame_factor` bytes.
//! * 10 or 12 bytes: the extra data of RealMedia `atrc` streams (big-endian version, samples per
//!   frame, delay and coding mode). Frames of these streams are scrambled and descrambled by the
//!   decoder.
//!
//! Joint stereo streams (coding mode 0x12) and multichannel streams are supported.

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

mod atrac1;
mod atrac3;
mod common;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_ATRAC1, CODEC_ID_ATRAC3};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use atrac1::{Atrac1Core, SU_SAMPLES};
use atrac3::{Atrac3Config, Atrac3Core, SAMPLES_PER_FRAME};

/// The frame decoder of a codec.
enum Core {
    Atrac1(Box<Atrac1Core>),
    Atrac3(Box<Atrac3Core>),
}

impl Core {
    /// Number of samples per channel of a frame.
    fn samples_per_frame(&self) -> usize {
        match self {
            Core::Atrac1(_) => SU_SAMPLES,
            Core::Atrac3(_) => SAMPLES_PER_FRAME,
        }
    }
}

/// ATRAC1 and ATRAC3 decoder.
pub struct AtracDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    codec_info: CodecInfo,
    buf: AudioBuffer<f32>,
    core: Core,
    /// The samples of the frame being decoded, one vector per channel.
    frame: Vec<Vec<f32>>,
}

impl AtracDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let Some(supported) = Self::supported_codecs().iter().find(|c| c.id == params.codec)
        else {
            return unsupported_error("atrac: invalid codec");
        };

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("atrac: sample rate is required"),
        };

        let Some(channels) = params.channels.clone()
        else {
            return unsupported_error("atrac: channels are required");
        };

        let num_channels = channels.count();

        let core = match params.codec {
            CODEC_ID_ATRAC1 => {
                if num_channels < 1 || num_channels > atrac1::MAX_CHANNELS {
                    return unsupported_error("atrac1: unsupported number of channels");
                }
                Core::Atrac1(Box::new(Atrac1Core::new(num_channels)?))
            }
            _ => {
                let Some(extra) = params.extra_data.as_deref()
                else {
                    return unsupported_error("atrac3: extra data is required");
                };
                let config = Atrac3Config::parse(extra, num_channels)?;
                Core::Atrac3(Box::new(Atrac3Core::new(config, num_channels)?))
            }
        };

        let samples = core.samples_per_frame();

        let spec = AudioSpec::new(rate, channels);
        let buf = AudioBuffer::new(spec, samples);

        Ok(AtracDecoder {
            params: params.clone(),
            opts: *opts,
            codec_info: supported.info,
            buf,
            core,
            frame: vec![vec![0.0; samples]; num_channels],
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        // The number of bytes of a frame and the number of frames in the packet.
        let (frame_bytes, num_frames) = match &self.core {
            Core::Atrac1(core) => (core.frame_bytes(), data.len() / core.frame_bytes()),
            // The frame size of ATRAC3 is the block align: the size of the packet.
            Core::Atrac3(_) => (data.len(), usize::from(!data.is_empty())),
        };

        if num_frames == 0 {
            return decode_error("atrac: packet is too small");
        }

        let samples = self.core.samples_per_frame();

        if num_frames * samples > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), num_frames * samples);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(num_frames * samples));

        for (n, frame_data) in data.chunks_exact(frame_bytes).take(num_frames).enumerate() {
            match &mut self.core {
                Core::Atrac1(core) => core.decode_frame(frame_data, &mut self.frame)?,
                Core::Atrac3(core) => core.decode_frame(frame_data, &mut self.frame)?,
            }

            for (ch, samples_ch) in self.frame.iter().enumerate() {
                let Some(plane) = self.buf.plane_mut(ch)
                else {
                    return decode_error("atrac: missing output plane");
                };
                plane[n * samples..(n + 1) * samples].copy_from_slice(samples_ch);
            }
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for AtracDecoder {
    fn reset(&mut self) {
        match &mut self.core {
            Core::Atrac1(core) => core.reset(),
            Core::Atrac3(core) => core.reset(),
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        &self.codec_info
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

impl RegisterableAudioDecoder for AtracDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AtracDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(
                CODEC_ID_ATRAC1,
                "atrac1",
                "ATRAC1 (Adaptive TRansform Acoustic Coding)"
            ),
            support_audio_codec!(
                CODEC_ID_ATRAC3,
                "atrac3",
                "ATRAC3 (Adaptive TRansform Acoustic Coding 3)"
            ),
        ]
    }
}
