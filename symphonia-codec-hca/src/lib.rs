// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::large_const_arrays)]
#![allow(clippy::manual_range_contains)]

//! CRI HCA (High Compression Audio) decoder.
//!
//! The decoder follows FFmpeg's `libavcodec/hcadec.c` (see `NOTICE`). The codec extra data is the
//! header of the stream, from the `HCA` signature to the end of its chunks, and a packet is a
//! frame (`block size` bytes). Each frame decodes to 1024 samples per channel.
//!
//! Like FFmpeg, the muted samples at the start and the end of the stream and the loop points are
//! not applied. A stream that is encrypted with a key is decoded as is unless the key is given
//! with [`HcaDecoder::try_new_with_key`].

mod cipher;
mod frame;
mod header;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_HCA;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::dsp::mdct::Imdct;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

pub use cipher::HcaKey;
use frame::{BitReader, Channel, apply_intensity_stereo};
use header::{BANDS, Header};
use tables::{ATH_BASE_CURVE, WINDOW};

/// The number of samples per channel of a frame: 8 subframes of 128 samples.
const FRAME_SAMPLES: usize = 8 * BANDS;

/// The absolute threshold of hearing of the bands for a sample rate (`ath_init1`).
fn ath_table(ath_type: u16, sample_rate: u32) -> Result<[u8; BANDS]> {
    let mut ath = [0u8; BANDS];

    match ath_type {
        0 => (),
        1 => {
            let mut acc = 0u32;

            for i in 0..BANDS {
                acc = acc.wrapping_add(sample_rate);
                let index = (acc >> 13) as usize;

                if index >= 654 {
                    ath[i..].fill(0xff);
                    break;
                }

                ath[i] = ATH_BASE_CURVE[index];
            }
        }
        _ => return decode_error("hca: invalid ATH type"),
    }

    Ok(ath)
}

/// The type of each channel: 0 is a lone channel, 1 and 2 are the primary and the secondary
/// channel of a pair coded with intensity stereo.
fn channel_types(header: &Header) -> [u8; header::MAX_CHANNELS] {
    let mut r = [0u8; header::MAX_CHANNELS];
    let tracks = header.track_count;
    let b = header.channels / tracks;

    if header.stereo_band_count > 0 && b > 1 {
        for t in 0..tracks {
            let x = &mut r[t * b..(t + 1) * b];

            match b {
                2 | 3 => {
                    x[0] = 1;
                    x[1] = 2;
                }
                4 => {
                    x[0] = 1;
                    x[1] = 2;
                    if header.channel_config == 0 {
                        x[2] = 1;
                        x[3] = 2;
                    }
                }
                5 => {
                    x[0] = 1;
                    x[1] = 2;
                    if header.channel_config <= 2 {
                        x[3] = 1;
                        x[4] = 2;
                    }
                }
                6 | 7 => {
                    x[0] = 1;
                    x[1] = 2;
                    x[4] = 1;
                    x[5] = 2;
                }
                8 => {
                    x[0] = 1;
                    x[1] = 2;
                    x[4] = 1;
                    x[5] = 2;
                    x[6] = 1;
                    x[7] = 2;
                }
                _ => (),
            }
        }
    }

    r
}

/// CRI HCA decoder.
pub struct HcaDecoder {
    params: AudioCodecParameters,
    header: Header,
    ath: [u8; BANDS],
    cipher: [u8; 256],
    channels: Vec<Channel>,
    imdct: Imdct,
    /// The output of the transform of a subframe.
    imdct_out: Vec<f32>,
    buf: AudioBuffer<f32>,
}

impl HcaDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        Self::try_new_with_key(params, opts, None)
    }

    /// Instantiates a decoder of a stream that is encrypted with `key`.
    pub fn try_new_with_key(
        params: &AudioCodecParameters,
        _opts: &AudioDecoderOptions,
        key: Option<HcaKey>,
    ) -> Result<Self> {
        if params.codec != CODEC_ID_HCA {
            return unsupported_error("hca: invalid codec");
        }

        let Some(extra) = params.extra_data.as_deref()
        else {
            return unsupported_error("hca: missing header");
        };

        let header = Header::parse(extra)?;
        let ath = ath_table(header.ath_type, header.sample_rate)?;

        let types = channel_types(&header);
        let mut channels = Vec::with_capacity(header.channels);

        for &t in &types[..header.channels] {
            let count = header.base_band_count + if t != 2 { header.stereo_band_count } else { 0 };
            if count > BANDS {
                return decode_error("hca: invalid band layout");
            }
            channels.push(Channel::new(t, count));
        }

        let mut cipher = std::array::from_fn(|i| i as u8);
        if let Some(table) = key.and_then(|k| k.cipher_table()) {
            cipher = table;
        }

        let layout = match header.channels {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            n => Channels::Discrete(n as u16),
        };

        let spec = AudioSpec::new(header.sample_rate, layout.clone());
        let buf = AudioBuffer::new(spec, FRAME_SAMPLES);

        let mut params = params.clone();
        params.with_sample_rate(header.sample_rate).with_channels(layout);

        Ok(HcaDecoder {
            params,
            header,
            ath,
            cipher,
            channels,
            imdct: Imdct::new_scaled(BANDS, IMDCT_SCALE),
            imdct_out: vec![0.0; 2 * BANDS],
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let h = &self.header;
        let data = packet.data;

        // The frame, in full, is covered by the checksum.
        if data.len() != usize::from(h.block_size) {
            return decode_error("hca: invalid frame size");
        }

        let mut br = BitReader::new(data, &self.cipher);

        if br.crc() != 0 {
            return decode_error("hca: frame checksum mismatch");
        }

        if br.read(16) != 0xffff {
            return decode_error("hca: invalid frame sync");
        }

        let packed_noise_level = ((br.read(9) << 8) as i32) - br.read(7) as i32;

        for ch in &mut self.channels {
            ch.unpack(&mut br, h.hfr_group_count, packed_noise_level, &self.ath);
        }

        if br.overrun() {
            return decode_error("hca: frame overrun");
        }

        self.buf.clear();
        self.buf.render_uninit(Some(FRAME_SAMPLES));

        let n = self.channels.len();

        for sub in 0..8 {
            for ch in &mut self.channels {
                ch.dequantize(&mut br);
            }

            for ch in &mut self.channels {
                ch.reconstruct_hfr(
                    h.stereo_band_count + h.base_band_count,
                    h.hfr_group_count,
                    h.bands_per_hfr_group,
                    h.total_band_count,
                );
            }

            for i in 0..n.saturating_sub(1) {
                let (a, b) = self.channels.split_at_mut(i + 1);
                apply_intensity_stereo(
                    &mut a[i],
                    &mut b[0],
                    sub,
                    h.total_band_count - h.base_band_count,
                    h.base_band_count,
                    h.stereo_band_count,
                );
            }

            for (c, ch) in self.channels.iter_mut().enumerate() {
                self.imdct.imdct(&ch.coefs, &mut self.imdct_out);
                let transformed = &self.imdct_out[IMDCT_OFFSET..IMDCT_OFFSET + BANDS];

                let Some(plane) = self.buf.plane_mut(c)
                else {
                    return decode_error("hca: missing plane");
                };
                let out = &mut plane[sub * BANDS..(sub + 1) * BANDS];
                let half = BANDS / 2;

                // `vector_fmul_window` of the previous and the current transform.
                for k in 0..half {
                    let s0 = ch.prev[half + k];
                    let s1 = transformed[half - 1 - k];
                    let wi = WINDOW[k];
                    let wj = WINDOW[BANDS - 1 - k];
                    out[k] = s0 * wj - s1 * wi;
                    out[BANDS - 1 - k] = s0 * wi + s1 * wj;
                }

                ch.prev.copy_from_slice(transformed);
            }
        }

        Ok(())
    }
}

/// The scale of the transform. FFmpeg scales its half transform by `1 / 8`; the transform of
/// Symphonia has the opposite sign (found by comparing the output with FFmpeg's).
const IMDCT_SCALE: f64 = -1.0 / 8.0;

/// The first output of the transform that is the output of FFmpeg's half transform.
const IMDCT_OFFSET: usize = BANDS / 2;

impl AudioDecoder for HcaDecoder {
    fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
        self.buf.clear();
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

impl RegisterableAudioDecoder for HcaDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(HcaDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_HCA, "hca", "CRI HCA")]
    }
}
