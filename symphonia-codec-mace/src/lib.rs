// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Apple MACE (Macintosh Audio Compression/Expansion) 3:1 and 6:1 decoder.
//!
//! MACE is a tiny 8-bit adaptive-quantizer codec from classic Mac OS (AIFF-C `MAC3`/`MAC6`,
//! QuickTime `MAC3`/`MAC6`, CAF `MAC3`/`MAC6`). There is no public specification; this decoder
//! is a port of FFmpeg's `libavcodec/mace.c` (see `NOTICE`), including its bit-exact
//! "broken clip" quirk that mirrors the original binary decoder.
//!
//! Both variants decode blocks of 6 frames. A MACE 3:1 block is 2 bytes per channel (3 samples
//! per byte), a MACE 6:1 block is 1 byte per channel (6 samples per byte), and the blocks of the
//! channels (mono or stereo) are interleaved byte-wise. A packet holds one or more whole blocks;
//! trailing bytes that do not make a whole block are ignored, like the reference decoder.

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

mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef, layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MACE3, CODEC_ID_MACE6};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use tables::{TAB1, TAB2, TAB3, TAB4};

/// The number of frames decoded from one block.
const BLOCK_FRAMES: usize = 6;
/// The number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 4096;

/// The per-channel decoder state (`ChannelData` in FFmpeg).
#[derive(Clone, Copy, Default)]
struct ChannelState {
    index: i16,
    factor: i16,
    prev2: i16,
    previous: i16,
    level: i16,
}

/// MACE's clip to 16 bit. Unlike a regular clip, it saturates to -32767, which keeps the output
/// identical to the original binary decoder.
fn broken_clip_int16(n: i32) -> i16 {
    if n > 32767 {
        32767
    }
    else if n < -32768 {
        -32767
    }
    else {
        n as i16
    }
}

/// Expands the 8-bit quantity in the upper byte of `x` to 16 bit by replicating it in the low
/// byte (`QT_8S_2_16S`).
fn expand_8s_16s(x: i32) -> i16 {
    (((x & 0xff00) | ((x >> 8) & 0xff)) & 0xffff) as u16 as i16
}

impl ChannelState {
    /// Looks up the quantizer level of the 3-bit (`wide`) or 2-bit code `val`, then adapts the
    /// step-size index.
    fn read_table(&mut self, val: u8, wide: bool) -> i16 {
        let row = usize::from(((self.index & 0x7f0) >> 4) as u16);
        let val = usize::from(val);

        let (current, step) = if wide {
            // `val` is a 3-bit code.
            let current = if val < 4 { TAB2[row][val] } else { -1 - TAB2[row][7 - val] };
            (current, TAB1[val & 7])
        }
        else {
            // `val` is a 2-bit code.
            let current = if val < 2 { TAB4[row][val] } else { -1 - TAB4[row][3 - val] };
            (current, TAB3[val & 3])
        };

        let index = i32::from(self.index) + i32::from(step) - i32::from(self.index >> 5);
        self.index = if index < 0 { 0 } else { index as i16 };

        current
    }

    /// Decodes one MACE 3:1 code into one sample (`chomp3`). `slot` is the position of the code
    /// in its group of 3 (the middle code is a 2-bit code).
    fn chomp3(&mut self, val: u8, slot: usize) -> i16 {
        let current = self.read_table(val, slot != 1);
        let current = broken_clip_int16(i32::from(current) + i32::from(self.level));

        self.level = current - (current >> 3);
        expand_8s_16s(i32::from(current))
    }

    /// Decodes one MACE 6:1 code into two samples (`chomp6`).
    fn chomp6(&mut self, val: u8, slot: usize) -> [i16; 2] {
        let current = self.read_table(val, slot != 1);

        if (self.previous ^ current) >= 0 {
            self.factor = (i32::from(self.factor) + 506).min(32767) as i16;
        }
        else if i32::from(self.factor) - 314 < -32768 {
            self.factor = -32767;
        }
        else {
            self.factor -= 314;
        }

        let current = broken_clip_int16(i32::from(current) + i32::from(self.level));

        self.level = ((i32::from(current) * i32::from(self.factor)) >> 15) as i16;
        let current = i32::from(current >> 1);

        let previous = i32::from(self.previous);
        let prev2 = i32::from(self.prev2);

        let out = [
            expand_8s_16s(previous + prev2 - ((prev2 - current) >> 2)),
            expand_8s_16s(previous + current + ((prev2 - current) >> 2)),
        ];

        self.prev2 = self.previous;
        self.previous = current as i16;

        out
    }
}

/// Which MACE variant is decoded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Mace3,
    Mace6,
}

impl Variant {
    /// The number of input bytes per channel in a block of 6 frames.
    fn block_bytes_per_channel(self) -> usize {
        match self {
            Variant::Mace3 => 2,
            Variant::Mace6 => 1,
        }
    }
}

/// MACE 3:1 and 6:1 decoder.
pub struct MaceDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    variant: Variant,
    channels: usize,
    buf: AudioBuffer<i16>,
    state: [ChannelState; 2],
}

impl MaceDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        let variant = match params.codec {
            CODEC_ID_MACE3 => Variant::Mace3,
            CODEC_ID_MACE6 => Variant::Mace6,
            _ => return unsupported_error("mace: invalid codec"),
        };

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            _ => return unsupported_error("mace: sample rate is required"),
        };

        let (channels, layout): (usize, Channels) = match &params.channels {
            Some(channels) if channels.count() == 1 => (1, layouts::CHANNEL_LAYOUT_MONO),
            Some(channels) if channels.count() == 2 => (2, layouts::CHANNEL_LAYOUT_STEREO),
            Some(_) => return unsupported_error("mace: only mono and stereo are supported"),
            None => return unsupported_error("mace: channels are required"),
        };

        let spec = AudioSpec::new(rate, layout.clone());
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES);

        let mut params = params.clone();
        params.with_channels(layout);

        Ok(MaceDecoder { params, opts: *opts, variant, channels, buf, state: Default::default() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let channels = self.channels;
        let block_len = channels * self.variant.block_bytes_per_channel();

        // Like the reference decoder, ignore trailing bytes that do not make a whole block.
        let blocks = packet.data.len() / block_len;

        if blocks == 0 {
            return decode_error("mace: packet is too small");
        }

        let frames = blocks * BLOCK_FRAMES;

        if frames > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames));

        let data = &packet.data[..blocks * block_len];
        let per_channel = self.variant.block_bytes_per_channel();

        for ch in 0..channels {
            let Some(out) = self.buf.plane_mut(ch)
            else {
                return decode_error("mace: missing output plane");
            };
            let state = &mut self.state[ch];
            let mut out = out[..frames].iter_mut();

            for block in data.chunks_exact(block_len) {
                for &pkt in &block[ch * per_channel..(ch + 1) * per_channel] {
                    // The codes of a byte in decoding order. MACE 3:1 reads the byte
                    // least-significant code first; MACE 6:1 most-significant code first.
                    let vals = match self.variant {
                        Variant::Mace3 => [pkt & 7, (pkt >> 3) & 3, pkt >> 5],
                        Variant::Mace6 => [pkt >> 5, (pkt >> 3) & 3, pkt & 7],
                    };

                    for (slot, &val) in vals.iter().enumerate() {
                        match self.variant {
                            Variant::Mace3 => {
                                if let Some(o) = out.next() {
                                    *o = state.chomp3(val, slot);
                                }
                            }
                            Variant::Mace6 => {
                                for s in state.chomp6(val, slot) {
                                    if let Some(o) = out.next() {
                                        *o = s;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for MaceDecoder {
    fn reset(&mut self) {
        self.state = Default::default();
    }

    fn codec_info(&self) -> &CodecInfo {
        let id = match self.variant {
            Variant::Mace3 => CODEC_ID_MACE3,
            Variant::Mace6 => CODEC_ID_MACE6,
        };
        // Both variants are registered, so the lookup always succeeds.
        Self::supported_codecs()
            .iter()
            .find(|c| c.id == id)
            .map(|c| &c.info)
            .unwrap_or(&Self::supported_codecs()[0].info)
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

impl RegisterableAudioDecoder for MaceDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(MaceDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[
            support_audio_codec!(CODEC_ID_MACE3, "mace3", "MACE 3:1"),
            support_audio_codec!(CODEC_ID_MACE6, "mace6", "MACE 6:1"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(codec: symphonia_core::codecs::audio::AudioCodecId, ch: usize) -> MaceDecoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(codec).with_sample_rate(22050).with_channels(if ch == 1 {
            layouts::CHANNEL_LAYOUT_MONO
        }
        else {
            layouts::CHANNEL_LAYOUT_STEREO
        });
        MaceDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    /// Arbitrary (corrupt) input must never panic and must yield the expected frame count.
    #[test]
    fn verify_random_packets_do_not_panic() {
        for (codec, ch) in
            [(CODEC_ID_MACE3, 1), (CODEC_ID_MACE3, 2), (CODEC_ID_MACE6, 1), (CODEC_ID_MACE6, 2)]
        {
            let mut dec = decoder(codec, ch);
            let mut state = 0x1234_5678_9abc_def0u64;
            let mut data = vec![0u8; 4 * 2 * ch * 5 + 1];
            for _ in 0..2000 {
                for b in data.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state >> 32) as u8;
                }
                let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
                let buf = dec.decode_ref(&pkt).unwrap();
                let blocks = data.len() / (ch * if codec == CODEC_ID_MACE3 { 2 } else { 1 });
                assert_eq!(buf.frames(), blocks * BLOCK_FRAMES);
            }
        }
    }

    #[test]
    fn verify_short_packet_is_error() {
        let mut dec = decoder(CODEC_ID_MACE3, 2);
        let data = [0u8; 3];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
        assert_eq!(dec.last_decoded().frames(), 0);
    }

    #[test]
    fn verify_more_than_two_channels_is_unsupported() {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_MACE6)
            .with_sample_rate(22050)
            .with_channels(layouts::CHANNEL_LAYOUT_5P1);
        assert!(MaceDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
    }
}
