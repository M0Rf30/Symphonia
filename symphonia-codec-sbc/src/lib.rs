// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The synthesis mirrors the fixed-point reference, and its index based loops keep the order of
// the arithmetic obvious.
#![allow(clippy::needless_range_loop)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust SBC (Low Complexity Subband Coding) decoder.
//!
//! SBC is the mandatory codec of the Bluetooth Advanced Audio Distribution Profile (A2DP), and,
//! in its fixed-parameter "mSBC" form, of the wide-band speech of the Hands-Free Profile (HFP).
//!
//! The decoder reproduces FFmpeg's fixed-point decoder (`libavcodec/sbcdec.c`) bit for bit. A
//! packet holds one frame (the sync word is `0x9c`, or `0xad` for mSBC); any bytes after the
//! first frame are ignored, as in FFmpeg. The sample rate, the channel mode and the number of
//! samples are taken from the header of each frame, and the output is 16-bit planar.
//!
//! [`FrameHeader`] parses frame headers, and gives the length of a frame, so that a stream of
//! consecutive frames can be split.

mod frame;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_SBC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

pub use frame::{FrameHeader, MSBC_FRAME_LEN, MSBC_SYNCWORD, Mode, SBC_SYNCWORD};
use frame::{UnpackError, Unpacked, unpack_frame};
use tables::{PROTO_4_40M0, PROTO_4_40M1, PROTO_8_80M0, PROTO_8_80M1, SYN_MATRIX_4, SYN_MATRIX_8};

/// The most samples per channel of a frame: 16 blocks of 8 sub-bands.
const MAX_SAMPLES: usize = 128;

/// The synthesis filter state of a channel (`sbc_decoder_state`).
#[derive(Clone)]
struct Synth {
    v: [i32; 170],
    offset: [usize; 16],
}

impl Synth {
    fn new() -> Self {
        let mut offset = [0; 16];
        for (i, o) in offset.iter_mut().enumerate() {
            *o = 10 * i + 10;
        }
        Synth { v: [0; 170], offset }
    }

    /// Synthesize one block of 4 sub-band samples to 4 PCM samples (`sbc_synthesize_four`).
    fn four(&mut self, sb: &[i32; 8], out: &mut [i16]) {
        for i in 0..8 {
            // Shifting.
            if self.offset[i] == 0 {
                self.offset[i] = 79;
                self.v.copy_within(0..9, 80);
            }
            else {
                self.offset[i] -= 1;
            }

            // Distribute the new matrix value to the shifted position. The arithmetic wraps, like
            // the unsigned arithmetic of the reference.
            let m = &SYN_MATRIX_4[4 * i..4 * i + 4];
            let mut acc = 0i32;
            for k in 0..4 {
                acc = acc.wrapping_add(m[k].wrapping_mul(sb[k]));
            }
            self.v[self.offset[i]] = acc >> 15;
        }

        // Compute the samples.
        for i in 0..4 {
            let idx = 5 * i;
            let k = (i + 4) & 0xf;
            let mut acc = 0i32;

            for j in 0..5 {
                acc = acc
                    .wrapping_add(
                        self.v[self.offset[i] + 2 * j].wrapping_mul(PROTO_4_40M0[idx + j]),
                    )
                    .wrapping_add(
                        self.v[self.offset[k] + 2 * j + 1].wrapping_mul(PROTO_4_40M1[idx + j]),
                    );
            }

            out[i] = (acc >> 15).clamp(-0x8000, 0x7fff) as i16;
        }
    }

    /// Synthesize one block of 8 sub-band samples to 8 PCM samples (`sbc_synthesize_eight`).
    fn eight(&mut self, sb: &[i32; 8], out: &mut [i16]) {
        for i in 0..16 {
            if self.offset[i] == 0 {
                self.offset[i] = 159;
                self.v.copy_within(0..9, 160);
            }
            else {
                self.offset[i] -= 1;
            }

            let m = &SYN_MATRIX_8[8 * i..8 * i + 8];
            let mut acc = 0i32;
            for k in 0..8 {
                acc = acc.wrapping_add(m[k].wrapping_mul(sb[k]));
            }
            self.v[self.offset[i]] = acc >> 15;
        }

        for i in 0..8 {
            let idx = 5 * i;
            let k = (i + 8) & 0xf;
            let mut acc = 0i32;

            for j in 0..5 {
                acc = acc
                    .wrapping_add(
                        self.v[self.offset[i] + 2 * j].wrapping_mul(PROTO_8_80M0[idx + j]),
                    )
                    .wrapping_add(
                        self.v[self.offset[k] + 2 * j + 1].wrapping_mul(PROTO_8_80M1[idx + j]),
                    );
            }

            out[i] = (acc >> 15).clamp(-0x8000, 0x7fff) as i16;
        }
    }
}

fn channels_of(count: usize) -> Channels {
    if count == 1 {
        Channels::Positioned(Position::FRONT_LEFT)
    }
    else {
        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
    }
}

/// SBC decoder.
pub struct SbcDecoder {
    params: AudioCodecParameters,
    synth: [Synth; 2],
    frame: Box<Unpacked>,
    buf: AudioBuffer<i16>,
}

impl SbcDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        // Verify codec ID.
        if params.codec != CODEC_ID_SBC {
            return unsupported_error("sbc: invalid codec");
        }

        // The format of the stream is that of its frames, so the parameters only size the buffer
        // until the first frame.
        let rate = params.sample_rate.filter(|&r| r != 0).unwrap_or(44100);
        let channels = params.channels.as_ref().map_or(2, Channels::count);
        let channels = if channels == 1 { 1 } else { 2 };

        let spec = AudioSpec::new(rate, channels_of(channels));
        let buf = AudioBuffer::new(spec, MAX_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(rate);
        params.with_channels(channels_of(channels));

        Ok(SbcDecoder {
            params,
            synth: [Synth::new(), Synth::new()],
            frame: Box::new(Unpacked::new()),
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        match unpack_frame(packet.data, &mut self.frame) {
            Ok(_) => (),
            Err(UnpackError::TooShort) => return decode_error("sbc: truncated frame"),
            Err(UnpackError::Sync) => return decode_error("sbc: invalid sync word"),
            Err(UnpackError::Crc) => return decode_error("sbc: header CRC mismatch"),
            Err(UnpackError::Bitpool) => return decode_error("sbc: bitpool out of bounds"),
        }

        let h = self.frame.header;
        let channels = h.channels();
        let rate = h.sample_rate();

        // The channel mode and the sampling frequency may change from one frame to the next.
        if self.buf.spec().rate() != rate || self.buf.spec().channels().count() != channels {
            let spec = AudioSpec::new(rate, channels_of(channels));
            self.buf = AudioBuffer::new(spec, MAX_SAMPLES);
            self.params.with_sample_rate(rate);
            self.params.with_channels(channels_of(channels));
        }

        let samples = h.samples();
        self.buf.render_uninit(Some(samples));

        let step = h.subbands;

        for ch in 0..channels {
            let Some(plane) = self.buf.plane_mut(ch)
            else {
                return decode_error("sbc: missing plane");
            };

            for blk in 0..h.blocks {
                let sb = &self.frame.samples[blk][ch];
                let out = &mut plane[blk * step..(blk + 1) * step];

                if step == 4 {
                    self.synth[ch].four(sb, out);
                }
                else {
                    self.synth[ch].eight(sb, out);
                }
            }
        }

        Ok(())
    }
}

impl AudioDecoder for SbcDecoder {
    fn reset(&mut self) {
        self.synth = [Synth::new(), Synth::new()];
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

impl RegisterableAudioDecoder for SbcDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(SbcDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_SBC, "sbc", "SBC (low-complexity subband codec)")]
    }
}
