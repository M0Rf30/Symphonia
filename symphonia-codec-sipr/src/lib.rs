// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio SIPR (Sipro Lab ACELP.net, RealAudio 4.0/5.0) decoder.
//!
//! SIPR is a proprietary ACELP speech codec with four modes. The mode is told by the size of a
//! packet, which is exactly one coded unit (FFmpeg's `block_align`):
//!
//! | Packet  | Mode       | Sample rate | Samples per packet |
//! |---------|------------|-------------|--------------------|
//! | 20 B    | 16 kbit/s  | 16 kHz      | 160                |
//! | 19 B    | 8.5 kbit/s | 8 kHz       | 144                |
//! | 29 B    | 6.5 kbit/s | 8 kHz       | 288 (2 frames)     |
//! | 37 B    | 5.0 kbit/s | 8 kHz       | 480 (2 frames)     |
//!
//! The output is mono, 32-bit float. The sample rate of the stream parameters selects the
//! 16 kbit/s mode (16000 Hz) or the others (8000 Hz). The packets must already be deinterleaved
//! and bit-reordered, as the RealMedia demuxer does.
//!
//! There is no public specification: the decoder follows FFmpeg's `sipr.c` and `sipr16k.c` (see
//! `NOTICE`).

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
// The numeric kernels index several buffers in lock step, as the reference does.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

mod dsp;
mod low;
mod tables;
mod wide;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SIPR;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use low::{Low, Mode};
use wide::Wide;

/// The sample rate of the 16 kbit/s mode.
const WIDE_SAMPLE_RATE: u32 = 16000;

/// The sample rate of the other modes.
const NARROW_SAMPLE_RATE: u32 = 8000;

/// The largest number of samples in a packet (5.0 kbit/s: 2 frames of 5 subframes of 48).
const MAX_PACKET_SAMPLES: usize = 480;

/// The decoded parameters of a frame (`SiprParameters`).
#[derive(Default)]
pub(crate) struct Params {
    pub ma_pred_switch: usize,
    pub vq_indexes: [usize; 5],
    pub pitch_delay: [i32; 5],
    pub gp_index: [usize; 5],
    pub fc_indexes: [[i32; 10]; 5],
    pub gc_index: [usize; 5],
}

/// An LSB-first bit reader (FFmpeg's little-endian bitstream reader).
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Reads `n <= 16` bits; the bits past the end are 0.
    pub fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0;

        for i in 0..n as usize {
            let p = self.pos + i;
            let bit = self.buf.get(p >> 3).map_or(0, |b| (b >> (p & 7)) & 1);
            v |= u32::from(bit) << i;
        }

        self.pos += n as usize;
        v
    }
}

enum Core {
    Wide(Box<Wide>),
    Low(Box<Low>),
}

impl Core {
    /// The core of the mode with a packet of `len` bytes, if there is one.
    fn for_packet_len(wide: bool, len: usize) -> Option<Core> {
        match (wide, len) {
            (true, 20) => Some(Core::Wide(Box::new(Wide::new()))),
            (false, 19) => Some(Core::Low(Box::new(Low::new(Mode::M8k5)))),
            (false, 29) => Some(Core::Low(Box::new(Low::new(Mode::M6k5)))),
            (false, 37) => Some(Core::Low(Box::new(Low::new(Mode::M5k0)))),
            _ => None,
        }
    }

    fn packet_len(&self) -> usize {
        match self {
            Core::Wide(_) => wide::BITS_PER_FRAME / 8,
            Core::Low(l) => l.param().bits_per_frame / 8,
        }
    }

    /// Decodes a packet of exactly `packet_len` bytes, returning the number of samples.
    fn decode(&mut self, data: &[u8], out: &mut [f32]) -> usize {
        let mut r = Reader::new(data);
        let mut params = Params::default();

        match self {
            Core::Wide(w) => {
                wide::read_params(&mut r, &mut params);
                w.decode_frame(&params, out);
                wide::SAMPLES_PER_FRAME
            }
            Core::Low(l) => {
                let mut n = 0;

                for _ in 0..l.param().frames_per_packet {
                    low::read_params(&mut r, l.param(), &mut params);

                    let frame_size = l.param().subframe_count * 48;
                    l.decode_frame(&params, &mut out[n..n + frame_size]);
                    n += frame_size;
                }

                n
            }
        }
    }
}

/// RealAudio SIPR decoder.
pub struct SiprDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    wide: bool,
    buf: AudioBuffer<f32>,
    core: Option<Core>,
}

impl SiprDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_SIPR {
            return unsupported_error("sipr: invalid codec");
        }

        // Only mono is defined.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("sipr: only mono is supported");
            }
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => NARROW_SAMPLE_RATE,
        };

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        let buf = AudioBuffer::new(
            AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO),
            MAX_PACKET_SAMPLES,
        );

        Ok(SiprDecoder { params, opts: *opts, wide: rate == WIDE_SAMPLE_RATE, buf, core: None })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        // The mode follows the size of the packet. A change of size starts a new mode.
        if self.core.as_ref().is_none_or(|c| c.packet_len() != data.len()) {
            let Some(core) = Core::for_packet_len(self.wide, data.len())
            else {
                return decode_error("sipr: invalid packet size");
            };

            self.core = Some(core);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(MAX_PACKET_SAMPLES));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("sipr: missing output plane");
        };

        let Some(core) = self.core.as_mut()
        else {
            return decode_error("sipr: no decoder");
        };

        let n = core.decode(data, out);

        self.buf.truncate(n);

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for SiprDecoder {
    fn reset(&mut self) {
        self.core = None;
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

impl RegisterableAudioDecoder for SiprDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(SiprDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_SIPR, "sipr", "RealAudio SIPR (ACELP.net)")]
    }
}
