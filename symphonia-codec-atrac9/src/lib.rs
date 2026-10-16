// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC9 decoder.
//!
//! ATRAC9 (Adaptive TRansform Acoustic Coding 9) is Sony's codec for the PlayStation Vita and
//! PlayStation 4, and is used in many games and in `.at9` files. There is no public specification;
//! this decoder follows the structure of the FFmpeg `atrac9` decoder (see `NOTICE`).
//!
//! The decoder needs the 12 byte codec configuration (the "ATRAC9 config data" of the `.at9` WAVE
//! `fmt ` chunk) as codec extra data: a little-endian version, the magic byte `0xfe`, then the bit
//! fields `sample rate index (4)`, `channel config (3)`, `reserved (1)`, `frame bytes - 1 (11)`
//! and `log2 frames per superframe (2)`. The sample rate and channel layout are taken from it.
//!
//! A packet is a superframe of one, two or four frames of 64, 128 or 256 samples per channel
//! (depending on the sample rate). Each frame is a series of blocks (single channel, channel pair
//! or low-frequency effects) laid out according to the channel configuration. A block is a
//! quantized spectrum with scalefactors, optional intensity stereo and optional band extension,
//! transformed by an IMDCT.

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
mod block;
mod noise;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ATRAC9;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use bits::{BitReader, Codebooks};
use block::{BlockData, BlockType, Shared};
use tables::{SAMPLE_RATES, SRI_FRAME_LOG2};

/// The size in bytes of the codec configuration.
const CONFIG_LEN: usize = 12;

/// The number of blocks of the largest channel configuration.
const MAX_BLOCKS: usize = 5;

/// A channel configuration (FFmpeg's `ATRAC9BlockConfig`).
struct BlockConfig {
    channels: Channels,
    types: &'static [BlockType],
    /// The output plane of each channel of each block.
    plane_map: [[usize; 2]; MAX_BLOCKS],
}

fn block_config(idx: u32) -> Option<BlockConfig> {
    use BlockType::{Cpe, Lfe, Sce};

    Some(match idx {
        // Mono.
        0 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_MONO,
            types: &[Sce],
            plane_map: [[0, 0]; MAX_BLOCKS],
        },
        // Dual mono.
        1 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_STEREO,
            types: &[Sce, Sce],
            plane_map: [[0, 0], [1, 0], [0, 0], [0, 0], [0, 0]],
        },
        // Stereo.
        2 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_STEREO,
            types: &[Cpe],
            plane_map: [[0, 1], [0, 0], [0, 0], [0, 0], [0, 0]],
        },
        // 5.1.
        3 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_5P1_SIDE,
            types: &[Cpe, Sce, Lfe, Cpe],
            plane_map: [[0, 1], [2, 0], [3, 0], [4, 5], [0, 0]],
        },
        // 7.1.
        4 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_7P1,
            types: &[Cpe, Sce, Lfe, Cpe, Cpe],
            plane_map: [[0, 1], [2, 0], [3, 0], [4, 5], [6, 7]],
        },
        // Quad.
        5 => BlockConfig {
            channels: layouts::CHANNEL_LAYOUT_4P0_QUAD,
            types: &[Cpe, Cpe],
            plane_map: [[0, 1], [2, 3], [0, 0], [0, 0], [0, 0]],
        },
        _ => return None,
    })
}

/// The stream configuration parsed from the codec extra data.
struct Config {
    sample_rate: u32,
    samplerate_idx: usize,
    block_config: BlockConfig,
    /// The average size of a frame in bytes.
    avg_frame_size: usize,
    /// The number of frames in a superframe.
    frame_count: usize,
    /// log2 of the number of samples per channel of a frame.
    frame_log2: u32,
}

impl Config {
    fn parse(extra: &[u8]) -> Result<Config> {
        if extra.len() < CONFIG_LEN {
            return unsupported_error("atrac9: invalid extra data length");
        }

        let version = u32::from_le_bytes([extra[0], extra[1], extra[2], extra[3]]);
        if version > 2 {
            return unsupported_error("atrac9: unsupported version");
        }

        let mut br = BitReader::new(&extra[4..CONFIG_LEN]);

        if br.get(8) != 0xfe {
            return decode_error("atrac9: incorrect magic byte");
        }

        let samplerate_idx = br.get(4) as usize;
        let sample_rate = SAMPLE_RATES[samplerate_idx];

        let Some(block_config) = block_config(br.get(3))
        else {
            return decode_error("atrac9: incorrect block config");
        };

        if br.get1() {
            return decode_error("atrac9: incorrect verification bit");
        }

        let avg_frame_size = br.get(11) as usize + 1;

        let superframe_idx = br.get(2);
        if superframe_idx & 1 != 0 {
            return decode_error("atrac9: invalid superframe index");
        }

        Ok(Config {
            sample_rate,
            samplerate_idx,
            block_config,
            avg_frame_size,
            frame_count: 1 << superframe_idx,
            frame_log2: u32::from(SRI_FRAME_LOG2[samplerate_idx]),
        })
    }
}

/// ATRAC9 decoder.
pub struct Atrac9Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    config: Config,
    shared: Shared,
    blocks: Vec<Box<BlockData>>,
    imdct: Imdct,
    /// The IMDCT window.
    window: Box<[f32]>,
    /// The full-length IMDCT output.
    full: Box<[f32]>,
    /// The windowed output of one channel of one frame.
    out: Box<[f32]>,
}

impl Atrac9Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_ATRAC9 {
            return unsupported_error("atrac9: invalid codec");
        }

        let Some(extra) = params.extra_data.as_deref()
        else {
            return unsupported_error("atrac9: codec extra data is required");
        };

        let config = Config::parse(extra)?;
        let n = 1usize << config.frame_log2;

        let spec = AudioSpec::new(config.sample_rate, config.block_config.channels.clone());
        let buf = AudioBuffer::new(spec, n * config.frame_count);

        let mut params = params.clone();
        params
            .with_sample_rate(config.sample_rate)
            .with_channels(config.block_config.channels.clone());

        // The IMDCT window (FFmpeg's `imdct_win`).
        let window = (0..n)
            .map(|i| {
                let len = n as f32;
                let sidx = (i as f32 + 0.5) / len;
                let eidx = (len - i as f32 - 0.5) / len;
                let pi = std::f64::consts::PI;
                let s_c = f64::from((f64::from(sidx) * pi - pi / 2.0).sin() as f32) * 0.5 + 0.5;
                let e_c = f64::from((f64::from(eidx) * pi - pi / 2.0).sin() as f32) * 0.5 + 0.5;
                let (s_c, e_c) = (s_c as f32, e_c as f32);
                s_c / ((s_c * s_c) + (e_c * e_c))
            })
            .collect();

        let blocks = config.block_config.types.iter().map(|_| BlockData::new()).collect();

        Ok(Atrac9Decoder {
            params,
            opts: *opts,
            buf,
            shared: Shared::new(config.samplerate_idx, Codebooks::get()?),
            blocks,
            imdct: Imdct::new_scaled(n, IMDCT_SCALE),
            window,
            full: vec![0.0; 2 * n].into_boxed_slice(),
            out: vec![0.0; n].into_boxed_slice(),
            config,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let cfg = &self.config;
        let n = 1usize << cfg.frame_log2;
        let half = n >> 1;
        let frames = (packet.data.len() / cfg.avg_frame_size).min(cfg.frame_count);
        if frames == 0 {
            return decode_error("atrac9: packet is too small");
        }

        self.buf.clear();
        self.buf.render_silence(Some(n * frames));

        let mut br = BitReader::new(packet.data);

        for frame in 0..frames {
            for (j, &kind) in cfg.block_config.types.iter().enumerate() {
                let block = &mut self.blocks[j];
                block.decode(&mut self.shared, &mut br, kind)?;
                br.align();

                if br.overread() {
                    return decode_error("atrac9: packet is truncated");
                }

                for i in 0..usize::from(kind == BlockType::Cpe) + 1 {
                    let c = &mut block.channel[i];

                    // The IMDCT outputs the middle N samples of the 2N-sample transform.
                    self.imdct.imdct(&c.coeffs[..n], &mut self.full);
                    let temp = &self.full[half..half + n];

                    // Window and overlap-add with the previous frame (`vector_fmul_window`).
                    let win = &self.window;
                    for k in 0..half {
                        let j = half - 1 - k;
                        let (s0, s1) = (c.prev_win[k], temp[j]);
                        let (wi, wj) = (win[k], win[half + j]);
                        self.out[k] = s0 * wj - s1 * wi;
                        self.out[half + j] = s0 * wi + s1 * wj;
                    }
                    c.prev_win[..half].copy_from_slice(&temp[half..]);

                    // Symphonia's transform has the opposite sign convention of FFmpeg's
                    // AV_TX_FLOAT_MDCT, so negate the windowed samples.
                    for s in self.out.iter_mut() {
                        *s = -*s;
                    }

                    let plane = cfg.block_config.plane_map[j][i];
                    match self.buf.plane_mut(plane) {
                        Some(dst) => dst[frame * n..(frame + 1) * n].copy_from_slice(&self.out),
                        None => return decode_error("atrac9: invalid output plane"),
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

/// The scale of the IMDCT: FFmpeg initializes its transform with 1 / 32768.
const IMDCT_SCALE: f64 = 1.0 / 32768.0;

impl AudioDecoder for Atrac9Decoder {
    fn reset(&mut self) {
        for block in self.blocks.iter_mut() {
            block.reset_overlap();
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

impl RegisterableAudioDecoder for Atrac9Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Atrac9Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_ATRAC9, "atrac9", "ATRAC9")]
    }
}
