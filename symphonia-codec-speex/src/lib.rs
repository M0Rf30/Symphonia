// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The decoder is a line-by-line port of the floating-point decoder of libspeex, and its index
// based loops mirror the reference to keep the order of the arithmetic obvious.
#![allow(clippy::needless_range_loop)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust Speex decoder.
//!
//! Decodes narrow-band (8 kHz), wide-band (16 kHz) and ultra-wide-band (32 kHz) Speex (CELP and
//! sub-band CELP) streams, including all the sub-modes (the "quality" levels), the perceptual
//! post-filter ("enhancer"), discontinuous transmission, in-band signalling, several frames per
//! packet, and intensity stereo.
//!
//! The decoder is a port of the floating-point decoder of libspeex 1.2.1 (BSD-3-Clause, see the
//! `NOTICE` file), and reproduces its output sample for sample.
//!
//! # Codec parameters
//!
//! The stream is described by the 80-byte Speex header packet of Ogg Speex, which is expected in
//! `extra_data`. When it is absent, as for Speex in FLV, a single-frame-per-packet mono stream of
//! the mode matching the sample rate is assumed (as FFmpeg does).

mod bits;
mod dsp;
mod nb;
mod sb;
mod stereo;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bits::Bits;
use nb::{NB_FRAME_SIZE, NbDecoder, Status};
use sb::SbDecoder;
use stereo::StereoState;

/// The size of the Speex header packet (`sizeof(SpeexHeader)`).
const SPEEX_HEADER_SIZE: usize = 80;
/// The signature at the start of the Speex header packet.
const SPEEX_SIGNATURE: &[u8; 8] = b"Speex   ";
/// The only bit-stream version of Speex (`mode_bitstream_version`).
const SPEEX_BITSTREAM_VERSION: u32 = 4;
/// The most frames per packet that are accepted. Encoders write up to 10.
const MAX_FRAMES_PER_PACKET: u32 = 256;

/// The coding mode of a stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// Narrow-band, 160 samples per frame.
    Narrowband,
    /// Wide-band, 320 samples per frame.
    Wideband,
    /// Ultra-wide-band, 640 samples per frame.
    UltraWideband,
}

impl Mode {
    /// The number of samples per channel of a frame.
    fn frame_size(self) -> usize {
        match self {
            Mode::Narrowband => NB_FRAME_SIZE,
            Mode::Wideband => 2 * NB_FRAME_SIZE,
            Mode::UltraWideband => 4 * NB_FRAME_SIZE,
        }
    }

    /// The mode that `speexenc` selects for a sampling rate.
    fn from_sample_rate(rate: u32) -> Mode {
        if rate > 25000 {
            Mode::UltraWideband
        }
        else if rate > 12500 {
            Mode::Wideband
        }
        else {
            Mode::Narrowband
        }
    }
}

/// The fields of the Speex header packet that are needed to decode (`SpeexHeader`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct SpeexHeader {
    rate: u32,
    mode: Mode,
    nb_channels: u32,
    frames_per_packet: u32,
}

impl SpeexHeader {
    /// Parses and validates the header packet.
    fn read(buf: &[u8]) -> Result<SpeexHeader> {
        if buf.len() < SPEEX_HEADER_SIZE || buf[..8] != *SPEEX_SIGNATURE {
            return decode_error("speex: invalid header");
        }

        let u32_at = |offset: usize| {
            u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
        };

        // Field offsets: 8 speex_version[20], 28 speex_version_id, 32 header_size, 36 rate,
        // 40 mode, 44 mode_bitstream_version, 48 nb_channels, 52 bitrate, 56 frame_size, 60 vbr,
        // 64 frames_per_packet, 68 extra_headers.
        if u32_at(28) > 1 {
            return unsupported_error("speex: unsupported version");
        }

        let mode = match u32_at(40) {
            0 => Mode::Narrowband,
            1 => Mode::Wideband,
            2 => Mode::UltraWideband,
            _ => return decode_error("speex: invalid mode"),
        };

        // The reference refuses streams of both older and newer bit-stream versions.
        if u32_at(44) != SPEEX_BITSTREAM_VERSION {
            return unsupported_error("speex: unsupported bit-stream version");
        }

        // Anything but a mono stream is decoded as stereo, like the reference player does.
        let nb_channels = if u32_at(48) == 1 { 1 } else { 2 };

        // A header that says zero frames per packet is treated as one.
        let frames_per_packet = u32_at(64).max(1);
        if frames_per_packet > MAX_FRAMES_PER_PACKET {
            return unsupported_error("speex: too many frames per packet");
        }

        Ok(SpeexHeader { rate: u32_at(36), mode, nb_channels, frames_per_packet })
    }
}

/// The decoder of the sub-band layers.
enum Core {
    Nb(Box<NbDecoder>),
    Sb(Box<SbDecoder>),
}

impl Core {
    fn new(mode: Mode) -> Core {
        match mode {
            Mode::Narrowband => Core::Nb(Box::new(NbDecoder::new(false))),
            Mode::Wideband => Core::Sb(Box::new(SbDecoder::new_wideband())),
            Mode::UltraWideband => Core::Sb(Box::new(SbDecoder::new_ultra_wideband())),
        }
    }

    fn decode(
        &mut self,
        bits: &mut Bits<'_>,
        out: &mut [f32],
        stereo: Option<&mut StereoState>,
    ) -> std::result::Result<(), Status> {
        match self {
            Core::Nb(nb) => nb.decode(bits, out, None, stereo),
            Core::Sb(sb) => sb.decode(bits, out, None, stereo),
        }
    }
}

/// Speex decoder.
pub struct SpeexDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    mode: Mode,
    frames_per_packet: usize,
    core: Core,
    /// The state of the intensity stereo expansion, for stereo streams.
    stereo: Option<StereoState>,
    /// The mono frame that is expanded to stereo.
    mono: Vec<f32>,
    buf: AudioBuffer<f32>,
}

impl SpeexDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        // Verify codec ID.
        if params.codec != CODEC_ID_SPEEX {
            return unsupported_error("speex: invalid codec");
        }

        let header = match &params.extra_data {
            Some(extra_data) if extra_data.len() >= SPEEX_HEADER_SIZE => {
                SpeexHeader::read(extra_data)?
            }
            _ => {
                // No header: assume a mono stream with one frame per packet whose mode is that
                // of the sampling rate.
                let Some(rate) = params.sample_rate
                else {
                    return unsupported_error("speex: missing header and sample rate");
                };
                let channels = params.channels.as_ref().map_or(1, Channels::count);

                SpeexHeader {
                    rate,
                    mode: Mode::from_sample_rate(rate),
                    nb_channels: if channels == 1 { 1 } else { 2 },
                    frames_per_packet: 1,
                }
            }
        };

        let rate = params.sample_rate.filter(|&r| r != 0).unwrap_or(header.rate);
        if rate == 0 {
            return unsupported_error("speex: unknown sample rate");
        }

        let channels = if header.nb_channels == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        }
        else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        };

        let frame_size = header.mode.frame_size();
        let frames_per_packet = header.frames_per_packet as usize;

        let spec = AudioSpec::new(rate, channels.clone());
        let buf = AudioBuffer::new(spec, frame_size * frames_per_packet);

        let mut params = params.clone();
        params.with_sample_rate(rate);
        params.with_channels(channels);

        Ok(SpeexDecoder {
            params,
            opts: *opts,
            mode: header.mode,
            frames_per_packet,
            core: Core::new(header.mode),
            stereo: (header.nb_channels == 2).then(StereoState::default),
            mono: if header.nb_channels == 2 { vec![0.0; frame_size] } else { Vec::new() },
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let frame_size = self.mode.frame_size();

        self.buf.clear();
        self.buf.resize_uninit(frame_size * self.frames_per_packet);

        let mut bits = Bits::new(packet.data);
        let mut num_frames = 0;
        let mut status = Ok(());

        // Like `speexdec`, decode the number of frames the header announces, and stop at the
        // first one that is missing, invalid, or overruns the packet. The frames that were
        // decoded until then are kept.
        for _ in 0..self.frames_per_packet {
            let start = num_frames * frame_size;

            let res = match self.stereo.as_mut() {
                // Intensity stereo: decode the mono frame, which carries the stereo requests.
                Some(stereo) => self.core.decode(&mut bits, &mut self.mono, Some(stereo)),
                None => {
                    let plane = self.buf.plane_mut(0).expect("a mono plane");
                    self.core.decode(&mut bits, &mut plane[start..start + frame_size], None)
                }
            };

            if let Err(e) = res {
                status = Err(e);
                break;
            }

            // The frame overran the packet: discard it.
            if bits.remaining() < 0 {
                status = Err(Status::Corrupt);
                break;
            }

            if let Some(stereo) = self.stereo.as_mut() {
                let (left, right) = self.buf.plane_pair_mut(0, 1).expect("stereo planes");
                stereo.expand(
                    &self.mono,
                    &mut left[start..start + frame_size],
                    &mut right[start..start + frame_size],
                );
            }

            num_frames += 1;
        }

        if num_frames == 0 && status == Err(Status::Corrupt) {
            self.buf.clear();
            return decode_error("speex: invalid frame");
        }

        self.buf.truncate(num_frames * frame_size);

        // The decoder works with 16-bit sample values; scale to the [-1, 1] range.
        const SCALE: f32 = 1.0 / 32768.0;
        self.buf.apply(|s| {
            let s = s * SCALE;
            if s.is_finite() { s } else { 0.0 }
        });

        if self.opts.gapless {
            // The trailing padding is computed for a packet of the nominal number of frames. If
            // the packet holds fewer (the last packet of a stream), the missing frames already
            // account for part of the padding.
            let missing = (self.frames_per_packet - num_frames) * frame_size;
            let trim_end = (packet.trim_end.get() as usize).saturating_sub(missing);
            self.buf.trim(packet.trim_start.get() as usize, trim_end);
        }

        Ok(())
    }
}

impl AudioDecoder for SpeexDecoder {
    fn reset(&mut self) {
        self.core = Core::new(self.mode);
        if self.stereo.is_some() {
            self.stereo = Some(StereoState::default());
        }
        self.buf.clear();
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

impl RegisterableAudioDecoder for SpeexDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(SpeexDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_SPEEX, "speex", "Speex")]
    }
}
