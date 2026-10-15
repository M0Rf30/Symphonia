// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pure Rust iLBC (Internet Low Bitrate Codec, RFC 3951) decoder.
//!
//! Both frame lengths are supported: 20 ms (38 bytes, 160 samples) and 30 ms (50 bytes, 240
//! samples). The output is 8 kHz mono signed 16-bit PCM.
//!
//! The decoder is a port of FFmpeg's fixed-point `ilbc` decoder (see `NOTICE`) and is bit-exact
//! with it. Like FFmpeg, it
//!
//! * conceals a frame whose "empty frame" flag is set, or whose start sub-block is out of range,
//!   by replaying the previous residual through the previous synthesis filters, and
//! * does not run the RFC 3951 section 4.6 post-filter (the "enhancer"; the RFC makes it
//!   optional), nor the RFC pitch-based loss concealment, which is driven by the transport
//!   layer's loss indication and which Symphonia packets cannot carry.
//!
//! # Codec parameters
//!
//! The frame length is taken from `frames_per_block` or `max_frames_per_packet` when it is 160
//! or 240 samples (as set by the CAF and MP4/QuickTime demuxers). Otherwise it is detected from
//! the length of the first packet (a multiple of 38 or of 50 bytes). A packet may carry several
//! frames; a trailing partial frame is ignored.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// The decoder mirrors the index-based fixed-point reference.
#![allow(clippy::needless_range_loop)]

mod frame;
mod synth;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ILBC;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use frame::Mode;
use synth::Core;

/// The only sample rate of iLBC.
const SAMPLE_RATE: u32 = 8000;
/// Number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 8;

/// Gets the mode from a samples-per-frame value.
fn mode_from_samples(samples: Option<u64>) -> Option<Mode> {
    match samples {
        Some(160) => Some(Mode::Ms20),
        Some(240) => Some(Mode::Ms30),
        _ => None,
    }
}

/// Detects the mode from the length of a packet of whole frames.
fn mode_from_packet_len(len: usize) -> Option<Mode> {
    if len != 0 && len % Mode::Ms20.bytes() == 0 {
        Some(Mode::Ms20)
    }
    else if len != 0 && len % Mode::Ms30.bytes() == 0 {
        Some(Mode::Ms30)
    }
    else {
        None
    }
}

/// iLBC decoder.
pub struct IlbcDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    /// The mode given by the codec parameters, if any.
    declared: Option<Mode>,
    core: Option<Core>,
}

impl IlbcDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_ILBC {
            return unsupported_error("ilbc: invalid codec");
        }

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("ilbc: only mono is supported");
            }
        }

        let declared = mode_from_samples(params.frames_per_block)
            .or_else(|| mode_from_samples(params.max_frames_per_packet));

        let spec = AudioSpec::new(SAMPLE_RATE, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES * Mode::Ms30.samples());

        let mut params = params.clone();
        params.with_sample_rate(SAMPLE_RATE).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(IlbcDecoder { params, opts: *opts, buf, declared, core: declared.map(Core::new) })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // The declared mode is used, unless the packet is a whole number of frames of the other
        // mode only: containers may declare the wrong mode (FFmpeg writes 160 samples per packet
        // to CAF files of 30 ms streams), so the packet size decides then, as in FFmpeg.
        let len = packet.data.len();
        let mode = match (self.declared, &self.core) {
            (Some(mode), _)
                if len >= mode.bytes()
                    && (len % mode.bytes() == 0 || mode_from_packet_len(len).is_none()) =>
            {
                mode
            }
            (None, Some(core)) => core.mode(),
            _ => match mode_from_packet_len(len) {
                Some(mode) => mode,
                None => return decode_error("ilbc: packet is not a whole number of frames"),
            },
        };

        let frames = len / mode.bytes();
        if frames == 0 {
            return decode_error("ilbc: packet is too small");
        }

        if self.core.as_ref().is_none_or(|core| core.mode() != mode) {
            self.core = Some(Core::new(mode));
        }
        let Some(core) = self.core.as_mut()
        else {
            return decode_error("ilbc: missing core");
        };

        let samples = frames * mode.samples();
        if samples > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), samples);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(samples));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("ilbc: missing output plane");
        };

        for (data, out) in
            packet.data.chunks_exact(mode.bytes()).zip(out.chunks_exact_mut(mode.samples()))
        {
            core.decode_frame(data, out);
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for IlbcDecoder {
    fn reset(&mut self) {
        self.core = self.declared.map(Core::new);
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

impl RegisterableAudioDecoder for IlbcDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(IlbcDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_ILBC, "ilbc", "iLBC")]
    }
}
