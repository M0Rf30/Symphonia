// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AMR-WB (Adaptive Multi-Rate Wideband, ITU-T G.722.2) decoder.
//!
//! The decoder implements the nine speech modes (6.60 to 23.85 kbit/s) of 3GPP TS 26.190 in
//! 32-bit floating point, following the structure of FFmpeg's `amrwb` decoder (see `NOTICE`).
//! The output is 16 kHz mono, 320 samples per 20 ms frame.
//!
//! # Packet layout
//!
//! A packet holds exactly one frame in the "storage" / RFC 4867 octet-aligned layout used by the
//! `.awb` file format and by the MP4/3GP `sawb` sample entries:
//!
//! ```text
//! +----------------+---------------------------------+
//! | TOC byte       | speech bits (sorted, MSB first) |
//! +----------------+---------------------------------+
//! ```
//!
//! The table of contents byte is `P FT(4) Q P P`: the frame type (`FT`) selects the mode (0 to 8:
//! 6.60, 8.85, 12.65, 14.25, 15.85, 18.25, 19.85, 23.05 and 23.85 kbit/s), 9 for a comfort noise
//! (SID) frame, 14 for a lost frame and 15 for no data; `Q` is the frame quality indicator
//! (0 for a damaged frame). The speech bits are the class A, B and C bits in transmission order,
//! padded with zero bits to a whole byte.
//!
//! # Comfort noise, lost and damaged frames
//!
//! SID frames (type 9) carry the parameters of the background noise (3GPP TS 26.192, the
//! decoder-side of TS 26.193). A comfort noise frame is generated for a SID frame, and for "no
//! data", "speech lost" and damaged frames received while the comfort noise is active, as in the
//! OpenCORE AMR-WB decoder which is the reference for this implementation. The noise parameters
//! are interpolated between SID updates, dithered if the SID says the background noise is not
//! stationary, and faded out when no update is received for 50 frames. A SID that follows a
//! hangover period of the encoder is replaced with the average of the last decoded speech frames,
//! as specified.
//!
//! Frames with the type "no data" or "speech lost", and frames flagged as damaged (`Q = 0`), that
//! are received during speech are decoded as 320 samples of silence without touching the decoder
//! state (like FFmpeg). The specification suggests a "random signal" and "a muting technique";
//! the bad frame handling of the reference decoder (gain and pitch muting and pitch lag
//! concealment, TS 26.191) is not implemented. For the same reason, the first speech frame after
//! comfort noise is decoded normally, whereas the reference decoder limits its gain if the noise
//! was faded out. Reserved frame types (10 to 13) and packets shorter than the frame type requires
//! are decode errors.

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
#![allow(clippy::needless_range_loop)]

mod dtx;
mod lsp;
mod params;
mod synth;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_WB;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::dtx::{Rx, SidData, State};
use crate::synth::{Core, FRAME_SAMPLES};
use crate::tables::CF_SIZES_WB;

/// Sample rate of the decoded audio.
const SAMPLE_RATE: u32 = 16_000;

/// Frame type of a comfort noise (SID) frame.
const FT_SID: usize = 9;
/// Frame type of a lost speech frame.
const FT_SPEECH_LOST: usize = 14;
/// Frame type of a frame without data.
const FT_NO_DATA: usize = 15;

/// Adaptive Multi-Rate Wideband decoder.
pub struct AmrWbDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    core: Core,
}

impl AmrWbDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_AMR_WB {
            return unsupported_error("amrwb: invalid codec");
        }

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("amrwb: only mono is supported");
            }
        }

        let spec = AudioSpec::new(SAMPLE_RATE, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params
            .with_sample_rate(SAMPLE_RATE)
            .with_channels(layouts::CHANNEL_LAYOUT_MONO)
            .with_max_frames_per_packet(FRAME_SAMPLES as u64);

        Ok(AmrWbDecoder { params, opts: *opts, buf, core: Core::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        let Some(&toc) = data.first()
        else {
            return decode_error("amrwb: empty packet");
        };

        let frame_type = usize::from((toc >> 3) & 0xf);
        let good = toc & 0x4 != 0;

        // Classify the frame like the reference decoder (`mime_unsorting`).
        let payload = &data[1..];

        let rx = match frame_type {
            0..FT_SID if good => Rx::SpeechGood,
            0..FT_SID => Rx::SpeechBad,
            FT_SID if !good => Rx::SidBad,
            FT_SID => match SidData::is_update(payload) {
                Some(true) => Rx::SidUpdate,
                Some(false) => Rx::SidFirst,
                None => return decode_error("amrwb: truncated frame"),
            },
            FT_SPEECH_LOST => Rx::SpeechLost,
            FT_NO_DATA => Rx::NoData,
            _ => return decode_error("amrwb: reserved frame type"),
        };

        // The speech bits of a good frame must be complete.
        let speech = if matches!(rx, Rx::SpeechGood) {
            let size = usize::from(CF_SIZES_WB[frame_type]).div_ceil(8);
            match payload.get(..size) {
                Some(speech) => speech,
                None => return decode_error("amrwb: truncated frame"),
            }
        }
        else {
            payload
        };

        self.buf.clear();
        self.buf.render_uninit(Some(FRAME_SAMPLES));
        let Some(Ok(out)) = self.buf.plane_mut(0).map(<&mut [f32; FRAME_SAMPLES]>::try_from)
        else {
            return decode_error("amrwb: unexpected output buffer");
        };

        let state = self.core.rx_state(rx);

        if state != State::Speech {
            // Comfort noise, generated from the SID parameters, in place of a SID frame, and in
            // place of missing or damaged frames while the comfort noise is active.
            let sid = if rx == Rx::SidUpdate { SidData::parse(speech) } else { None };
            self.core.decode_comfort_noise(rx, state, sid.as_ref(), out);
        }
        else if rx == Rx::SpeechGood {
            let frame = params::unpack(frame_type, speech);
            self.core.decode_frame(frame_type, &frame, out);
        }
        else {
            // The specification suggests a "random signal" and "a muting technique" to gradually
            // decrease the output level; like FFmpeg, output silence.
            out.fill(0.0);
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for AmrWbDecoder {
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

impl RegisterableAudioDecoder for AmrWbDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AmrWbDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_AMR_WB, "amrwb", "Adaptive Multi-Rate Wideband")]
    }
}
