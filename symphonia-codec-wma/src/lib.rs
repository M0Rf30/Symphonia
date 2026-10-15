// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The decoder is a port of FFmpeg's, and its index based loops mirror the reference to keep the
// order of the arithmetic obvious.
#![allow(clippy::needless_range_loop)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust Windows Media Audio (WMA) decoder.
//!
//! Decodes WMA version 1 (WAVE format tag 0x0160, [`CODEC_ID_WMAV1`]) and version 2 (0x0161,
//! [`CODEC_ID_WMAV2`]) streams, which are MDCT transform codecs with a Huffman coded run/level
//! spectrum, LSP or Huffman coded spectral envelopes, perceptual noise substitution, joint
//! (mid/side) stereo, variable block lengths and a bit reservoir. WMA Pro, WMA Lossless and WMA
//! Voice are different codecs and are not decoded.
//!
//! The decoder is a port of FFmpeg's `wmadec.c`/`wma.c` (LGPL-2.1-or-later, see the `NOTICE`
//! file). There is no public specification of the bitstream.
//!
//! # Codec parameters
//!
//! The stream is described by the `WAVEFORMATEX` structure of the container, which is expected,
//! complete, in `extra_data`: the 18 byte header (format tag, channel count, sample rate, average
//! bytes per second, block alignment, bits per sample, size of the extension) followed by the
//! extension of the codec (4 bytes for version 1, 6 or 10 for version 2, whose last two bytes
//! before the end carry the encoder flags). The average byte rate, the block alignment and the
//! flags are not available elsewhere. The sample rate and the channel layout are taken from the
//! codec parameters when they are set.
//!
//! Every packet must be one block of `nBlockAlign` bytes (an ASF media object). Because of the
//! bit reservoir, a packet may complete the frame of the previous packet, so that a packet yields
//! from zero to fifteen frames of 512, 1024, or 2048 samples per channel.
//!
//! # Delay
//!
//! The overlap-add of the MDCT delays the output by two frames. When [`AudioDecoderOptions`]
//! `gapless` is set, the decoder discards the first `2 * frame_len` samples after it is created
//! or reset, as FFmpeg does. The `frame_len` samples that remain in the overlap buffer at the end
//! of the stream are not output.

mod bits;
mod core;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_WMAV1, CODEC_ID_WMAV2};
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bits::{Bits, PADDING};
use core::{Core, MAX_CHANNELS, StreamParams};

/// The size of the `WAVEFORMATEX` structure without its extension.
const WAVEFORMATEX_SIZE: usize = 18;
/// The maximum size of the coded data that is carried over to the next packet
/// (`MAX_CODED_SUPERFRAME_SIZE`).
const MAX_CODED_SUPERFRAME_SIZE: usize = 32768;
/// The maximum number of frames of a packet: the frame count is a 4 bit field.
const MAX_FRAMES_PER_PACKET: usize = 15;

/// The fields of the `WAVEFORMATEX` structure that the decoder needs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WaveFormat {
    channels: u16,
    sample_rate: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    /// The flags of the codec extension (`flags2` of FFmpeg), 0 if the extension is too short.
    flags: u16,
    /// The size of the codec extension.
    extension_size: usize,
}

impl WaveFormat {
    /// Parses the structure. `version` is 1 or 2.
    fn parse(data: &[u8], version: u8) -> Result<WaveFormat> {
        if data.len() < WAVEFORMATEX_SIZE {
            return decode_error("wma: invalid WAVEFORMATEX");
        }

        let u16_at = |pos: usize| u16::from_le_bytes([data[pos], data[pos + 1]]);
        let u32_at = |pos: usize| {
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
        };

        // The extension is `cbSize` bytes, but is also accepted when the container truncated it.
        let extension = &data[WAVEFORMATEX_SIZE..];
        let extension = &extension[..extension.len().min(usize::from(u16_at(16)))];

        // FFmpeg reads the flags from the bytes 2..4 of the extension for version 1 (with at least
        // 4 bytes), and 4..6 for version 2 (with at least 6 bytes).
        let flags = match version {
            1 if extension.len() >= 4 => u16::from_le_bytes([extension[2], extension[3]]),
            2 if extension.len() >= 6 => u16::from_le_bytes([extension[4], extension[5]]),
            _ => 0,
        };

        // For version 2, the flag set 0x000d does not use variable block lengths in streams that
        // have the longer 10 byte extension (FFmpeg issue 1503, `use_variable_block_len = 0`).
        let flags = if version == 2 && extension.len() >= 8 && flags == 0x000d {
            flags & !0x0004
        }
        else {
            flags
        };

        Ok(WaveFormat {
            channels: u16_at(2),
            sample_rate: u32_at(4),
            avg_bytes_per_sec: u32_at(8),
            block_align: u16_at(12),
            flags,
            extension_size: extension.len(),
        })
    }
}

/// Windows Media Audio 1 and 2 decoder.
pub struct WmaDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    core: Core,
    channels: usize,
    block_align: usize,
    buf: AudioBuffer<f32>,
    /// The packet followed by the padding.
    cur: Vec<u8>,
    /// The coded data of the frame that continues in the next packet (`last_superframe`), followed
    /// by the padding.
    last: Vec<u8>,
    /// The number of bytes of `last`.
    last_len: usize,
    /// The number of bits of the first byte of `last` that belong to the previous frame.
    last_bitoffset: u32,
    /// The number of output samples per channel that are still to be discarded.
    skip: usize,
}

impl WmaDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let version = match params.codec {
            CODEC_ID_WMAV1 => 1,
            CODEC_ID_WMAV2 => 2,
            _ => return unsupported_error("wma: invalid codec"),
        };

        let Some(extra_data) = params.extra_data.as_deref()
        else {
            return unsupported_error("wma: missing WAVEFORMATEX extra data");
        };

        let format = WaveFormat::parse(extra_data, version)?;

        if format.block_align == 0 {
            return unsupported_error("wma: block alignment is not set");
        }

        let sample_rate = params.sample_rate.unwrap_or(format.sample_rate);
        let num_channels = match &params.channels {
            Some(channels) => channels.count(),
            None => usize::from(format.channels),
        };

        if num_channels == 0 || num_channels > MAX_CHANNELS {
            return unsupported_error("wma: unsupported number of channels");
        }

        let core = Core::new(&StreamParams {
            version,
            sample_rate,
            channels: num_channels,
            bit_rate: format.avg_bytes_per_sec.saturating_mul(8),
            flags: format.flags,
        })?;

        let channels = if num_channels == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        }
        else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        };

        let spec = AudioSpec::new(sample_rate, channels.clone());
        let buf = AudioBuffer::new(spec, MAX_FRAMES_PER_PACKET * core.frame_len);

        let block_align = usize::from(format.block_align);

        let mut params = params.clone();
        params.with_sample_rate(sample_rate);
        params.with_channels(channels);

        let skip = if opts.gapless { 2 * core.frame_len } else { 0 };

        let cur = vec![0; block_align + PADDING];

        Ok(WmaDecoder {
            params,
            opts: *opts,
            core,
            channels: num_channels,
            block_align,
            buf,
            cur,
            last: Vec::with_capacity(MAX_CODED_SUPERFRAME_SIZE + block_align + PADDING),
            last_len: 0,
            last_bitoffset: 0,
            skip,
        })
    }

    /// Appends `bytes` to the coded data of the previous packet.
    fn push_last(&mut self, bytes: &[u8]) {
        self.last.truncate(self.last_len);
        self.last.extend_from_slice(bytes);
        self.last_len = self.last.len();
        self.last.extend_from_slice(&[0; PADDING]);
    }

    /// Decodes a packet (`wma_decode_superframe`).
    fn decode_inner(&mut self, data: &[u8]) -> Result<()> {
        let buf_size = self.block_align;

        if data.len() < buf_size {
            return decode_error("wma: packet is smaller than the block alignment");
        }

        self.cur.clear();
        self.cur.extend_from_slice(&data[..buf_size]);
        self.cur.extend_from_slice(&[0; PADDING]);

        let frame_len = self.core.frame_len;

        // Header: only the reservoir packets have one.
        let mut gb = Bits::new(&self.cur, 8 * buf_size as u64);

        let mut nb_frames = 1i32;

        if self.core.use_bit_reservoir {
            // Read the superframe header.
            gb.skip(4)?; // The superframe index.
            nb_frames = gb.read(4)? as i32 - i32::from(self.last_len == 0);

            if nb_frames <= 0 {
                // The packet holds no frame start: it is only the continuation of the frame of the
                // previous packets.
                if nb_frames < 0 || gb.left() <= 8 {
                    return decode_error("wma: invalid number of frames");
                }

                if self.last_len + buf_size - 1 > MAX_CODED_SUPERFRAME_SIZE {
                    return decode_error("wma: superframe is too large");
                }

                // FFmpeg adds the number of bits to the length in bytes here.
                self.push_last(&data[1..buf_size]);
                return Ok(());
            }
        }

        // The packet makes frames.
        self.buf.clear();
        self.buf.resize_uninit(nb_frames as usize * frame_len);
        let mut samples_offset = 0;

        if self.core.use_bit_reservoir {
            let header_bits = 4 + 4 + self.core.byte_offset_bits as usize + 3;
            let bit_offset = gb.read(self.core.byte_offset_bits + 3)? as usize;
            if bit_offset as i64 > gb.left() {
                return decode_error("wma: invalid last frame bit offset");
            }

            if self.last_len > 0 {
                // Add `bit_offset` bits to the last frame.
                if self.last_len + ((bit_offset + 7) >> 3) > MAX_CODED_SUPERFRAME_SIZE {
                    return decode_error("wma: superframe is too large");
                }

                self.last.truncate(self.last_len);
                let mut len = bit_offset;
                while len > 7 {
                    self.last.push(gb.read(8)? as u8);
                    len -= 8;
                }
                if len > 0 {
                    self.last.push((gb.read(len as u32)? << (8 - len)) as u8);
                }
                self.last.extend_from_slice(&[0; PADDING]);

                // The frame is stored in the last packet and in the current one.
                let mut last_gb =
                    Bits::new(&self.last, 8 * self.last_len as u64 + bit_offset as u64);
                // Skip the unused bits.
                last_gb.skip(self.last_bitoffset)?;
                self.core.decode_frame(&mut last_gb)?;
                emit_frame(&mut self.core, &mut self.buf, self.channels, samples_offset)?;
                samples_offset += frame_len;
                nb_frames -= 1;
            }

            // Read each frame starting from `bit_offset`.
            let pos = bit_offset + header_bits;
            if pos >= MAX_CODED_SUPERFRAME_SIZE * 8 || pos > buf_size * 8 {
                return decode_error("wma: invalid frame position");
            }

            let mut gb = Bits::new(&self.cur[pos >> 3..], 8 * (buf_size - (pos >> 3)) as u64);
            gb.skip((pos & 7) as u32)?;

            self.core.reset_block_lengths = true;
            for _ in 0..nb_frames {
                self.core.decode_frame(&mut gb)?;
                emit_frame(&mut self.core, &mut self.buf, self.channels, samples_offset)?;
                samples_offset += frame_len;
            }

            // Keep the end of the packet, after the last frame, for the frame that continues.
            let pos = gb.count() as usize + (pos & !7);
            self.last_bitoffset = (pos & 7) as u32;
            let pos = pos >> 3;
            if pos > buf_size {
                return decode_error("wma: invalid frame end");
            }
            self.last.clear();
            self.last_len = 0;
            self.push_last(&data[pos..buf_size]);
        }
        else {
            // Single frame decode.
            self.core.decode_frame(&mut gb)?;
            emit_frame(&mut self.core, &mut self.buf, self.channels, samples_offset)?;
        }

        Ok(())
    }

    /// Drops the leading samples that are still to be skipped, and the packet's own trimming.
    fn trim(&mut self, packet: &PacketRef<'_>) {
        let len = self.buf.frames();

        let skip = self.skip.min(len);
        self.skip -= skip;

        if self.opts.gapless {
            let start = skip + packet.trim_start.get() as usize;
            let end = packet.trim_end.get() as usize;
            self.buf.trim(start, end);
        }
    }
}

/// Moves the frame that was decoded into the planes of the buffer, at `offset`.
fn emit_frame(
    core: &mut Core,
    buf: &mut AudioBuffer<f32>,
    channels: usize,
    offset: usize,
) -> Result<()> {
    let frame_len = core.frame_len;
    let range = offset..offset + frame_len;

    if channels == 1 {
        let Some(plane) = buf.plane_mut(0).and_then(|p| p.get_mut(range))
        else {
            return decode_error("wma: output overflow");
        };
        core.take_frame(0, plane);
    }
    else {
        let Some((left, right)) = buf.plane_pair_mut(0, 1)
        else {
            return decode_error("wma: output overflow");
        };
        let (Some(left), Some(right)) = (left.get_mut(range.clone()), right.get_mut(range))
        else {
            return decode_error("wma: output overflow");
        };
        core.take_frame(0, left);
        core.take_frame(1, right);
    }

    Ok(())
}

impl AudioDecoder for WmaDecoder {
    fn reset(&mut self) {
        // The same as the `flush` callback of FFmpeg.
        self.last_len = 0;
        self.last.clear();
        self.last_bitoffset = 0;
        self.core.reset();
        self.skip = if self.opts.gapless { 2 * self.core.frame_len } else { 0 };
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        let id = self.params.codec;
        &Self::supported_codecs()
            .iter()
            .find(|c| c.id == id)
            .or_else(|| Self::supported_codecs().first())
            .expect("at least one codec registered")
            .info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        self.buf.clear();

        // The first decode calls are made with an empty buffer, and every error resets the bit
        // reservoir, as in FFmpeg.
        if let Err(e) = self.decode_inner(packet.data) {
            self.buf.clear();
            self.last.clear();
            self.last_len = 0;
            return Err(e);
        }

        self.trim(packet);

        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for WmaDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(WmaDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        const _: AudioCodecId = CODEC_ID_WMAV1;
        &[
            support_audio_codec!(CODEC_ID_WMAV1, "wmav1", "Windows Media Audio 1"),
            support_audio_codec!(CODEC_ID_WMAV2, "wmav2", "Windows Media Audio 2"),
        ]
    }
}
