// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC3+ decoder.
//!
//! ATRAC3+ (Adaptive TRansform Acoustic Coding 3+) is Sony's successor to ATRAC3, used by
//! MiniDisc/Hi-MD, PSP and Walkman devices and in `.oma`/`.aa3`/`.at3` files. There is no public
//! specification; this decoder follows the structure of the FFmpeg `atrac3plus` decoder (see
//! `NOTICE`).
//!
//! Each packet holds one frame of 2048 samples per channel. A frame is a series of channel units
//! (mono or stereo) that are decoded independently: a spectrum is Huffman-decoded and inverse
//! quantized, passed through an IMDCT per each of the 16 subbands, gain-compensated, combined with
//! synthesized sine waves, and merged by an inverse polyphase quadrature filter bank.
//!
//! One, two, three, four, six, seven and eight channel streams are supported. Channel units with
//! the extension unit type and the GHA "amplitude mode 0" tone coding are not supported (the
//! reference decoder does not support them either).

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
// Index loops mirror the structure of the reference implementation.
#![allow(clippy::needless_range_loop)]

mod bits;
mod codebooks;
mod dsp;
mod tables;
mod unit;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ATRAC3PLUS;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bits::BitReader;
use codebooks::Codebooks;
use dsp::Synth;
use unit::{
    CH_UNIT_EXTENSION, CH_UNIT_MONO, CH_UNIT_STEREO, CH_UNIT_TERMINATOR, ChanUnit, FRAME_SAMPLES,
    decode_channel_unit,
};

/// The channel layout of a stream: the channel unit types in bitstream order, and the output
/// plane of each decoded channel (FFmpeg's `set_channel_params` and `channel_map`).
struct Layout {
    channels: Channels,
    units: &'static [u32],
    map: &'static [usize],
}

fn layout_for(count: usize) -> Option<Layout> {
    const M: u32 = CH_UNIT_MONO;
    const S: u32 = CH_UNIT_STEREO;

    Some(match count {
        1 => Layout { channels: layouts::CHANNEL_LAYOUT_MONO, units: &[M], map: &[0] },
        2 => Layout { channels: layouts::CHANNEL_LAYOUT_STEREO, units: &[S], map: &[0, 1] },
        3 => Layout { channels: layouts::CHANNEL_LAYOUT_3P0, units: &[S, M], map: &[0, 1, 2] },
        4 => {
            Layout { channels: layouts::CHANNEL_LAYOUT_4P0, units: &[S, M, M], map: &[0, 1, 2, 3] }
        }
        6 => Layout {
            channels: layouts::CHANNEL_LAYOUT_5P1,
            units: &[S, M, S, M],
            map: &[0, 1, 2, 4, 5, 3],
        },
        7 => Layout {
            channels: layouts::CHANNEL_LAYOUT_6P1_HEX,
            units: &[S, M, S, M, M],
            map: &[0, 1, 2, 4, 5, 6, 3],
        },
        8 => Layout {
            channels: layouts::CHANNEL_LAYOUT_7P1,
            units: &[S, M, S, S, M],
            map: &[0, 1, 2, 4, 5, 6, 7, 3],
        },
        _ => return None,
    })
}

/// ATRAC3+ decoder.
pub struct Atrac3PlusDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    layout: Layout,
    codebooks: &'static Codebooks,
    units: Vec<Box<ChanUnit>>,
    synth: Synth,
}

impl Atrac3PlusDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_ATRAC3PLUS {
            return unsupported_error("atrac3plus: invalid codec");
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("atrac3plus: sample rate is required"),
        };

        let count = match &params.channels {
            Some(channels) => channels.count(),
            None => return unsupported_error("atrac3plus: channel count is required"),
        };

        let Some(layout) = layout_for(count)
        else {
            return unsupported_error("atrac3plus: unsupported channel count");
        };

        let spec = AudioSpec::new(rate, layout.channels.clone());
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params.with_channels(layout.channels.clone());

        let units = layout.units.iter().map(|_| ChanUnit::new()).collect();

        Ok(Atrac3PlusDecoder {
            params,
            opts: *opts,
            buf,
            layout,
            codebooks: Codebooks::get()?,
            units,
            synth: Synth::new(),
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();
        self.buf.render_silence(Some(FRAME_SAMPLES));

        let mut br = BitReader::new(packet.data);

        if br.get1() {
            return decode_error("atrac3plus: invalid start bit");
        }

        let mut block = 0;
        let mut out_ch = 0;

        while br.bits_left() >= 2 {
            let unit_id = br.get(2);

            if unit_id == CH_UNIT_TERMINATOR {
                break;
            }

            if unit_id == CH_UNIT_EXTENSION {
                return unsupported_error("atrac3plus: channel unit extension");
            }

            if self.layout.units.get(block) != Some(&unit_id) {
                return decode_error("atrac3plus: frame data doesn't match channel configuration");
            }

            let num_channels = unit_id as usize + 1;
            let unit = &mut self.units[block];
            unit.unit_type = unit_id;

            decode_channel_unit(&mut br, self.codebooks, unit, num_channels)?;

            if br.overread() {
                return decode_error("atrac3plus: packet is truncated");
            }

            self.synth.synthesize(unit, num_channels);

            for ch in 0..num_channels {
                let plane = self.layout.map[out_ch + ch];
                match self.buf.plane_mut(plane) {
                    Some(dst) => dst.copy_from_slice(&self.synth.output[ch]),
                    None => return decode_error("atrac3plus: invalid output plane"),
                }
            }

            block += 1;
            out_ch += num_channels;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for Atrac3PlusDecoder {
    fn reset(&mut self) {
        for unit in self.units.iter_mut() {
            *unit = ChanUnit::new();
        }
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

impl RegisterableAudioDecoder for Atrac3PlusDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Atrac3PlusDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_ATRAC3PLUS, "atrac3plus", "ATRAC3+")]
    }
}
