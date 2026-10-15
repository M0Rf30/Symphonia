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
#![allow(clippy::manual_range_contains)]
// Casts and indexing patterns inherent to the transform-codec arithmetic.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

//! Dolby E audio decoder.
//!
//! Dolby E is a professional codec that carries up to 8 channels (4.0, 5.1, or 7.1, with 1 to 8
//! programs) in the PCM of a pair of channels, as a SMPTE ST 337 data burst of a video frame.
//! There is no public specification of Dolby E: the decoder is a port of FFmpeg's decoder
//! (`libavcodec/dolby_e.c`, `dolby_e_parse.c`), and decodes the same streams to the same samples.
//!
//! # Packets
//!
//! A packet is a Dolby E frame, from its sync word: the payload words of a SMPTE ST 337 burst (see
//! [`symphonia_common::mpeg::s337m`]) in big-endian order, in the 16-bit, 20-bit (in 3 bytes), or
//! 24-bit form. A frame decodes to 1792 samples per channel.
//!
//! # Output
//!
//! Planar 32-bit floating-point samples in the range -1.0 to +1.0, in the Symphonia canonical
//! channel order: 4.0 (front left, front right, front centre, rear centre), 5.1 (front left,
//! front right, front centre, LFE, side left, side right), or 7.1 (front left, front right,
//! front centre, LFE, rear left, rear right, side left, side right). For a stream of a single
//! program, the channels are ordered as FFmpeg does by default; for a stream of several programs
//! of 8 channels in total, the channels are in the order in which they are coded (program 1
//! first).
//!
//! The sample rate reported is the rate at which the 1792 samples of a frame last as long as the
//! video frame (e.g., 44800 Hz for 25 frames per second), which is what FFmpeg reports; the PCM
//! that carries Dolby E runs at 48 kHz.
//!
//! # Notes
//!
//! * Delta bit allocation, and encoder revisions other than 0 and 1, are not supported, as in
//!   FFmpeg: the channel is silent.
//! * A channel that fails to parse is silent; a frame whose header, sizes, or metadata is invalid
//!   is a decode error.

mod bitalloc;
mod dsp;
mod frame;
mod tables;

use log::warn;
use symphonia_common::mpeg::dolby_e::{DolbyEHeader, FRAME_SAMPLES, MAX_CHANNELS, WordStream};
use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_DOLBY_E;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::dsp::{BLOCK_LEN, OVERLAP, SEGMENT_SAMPLES, TABLES, Transform};
use crate::frame::Frame;
use crate::tables::{CH_REORDER_4, CH_REORDER_6, CH_REORDER_8, CH_REORDER_N};

/// The default sample rate and channels, until the first frame tells.
const DEFAULT_RATE: u32 = 44_800;

/// The order in which the channels of a frame are output.
fn channel_order(header: &DolbyEHeader) -> &'static [u8] {
    match header.nb_channels {
        4 => &CH_REORDER_4,
        6 => &CH_REORDER_6,
        _ if header.nb_programs == 1 => &CH_REORDER_8,
        _ => &CH_REORDER_N,
    }
}

/// The gain of a channel, applied linearly across the frame from the gain at its start to the
/// gain at its end (both 10-bit gain codes, 960 being 0 dB).
fn apply_gain(begin: usize, end: usize, output: &mut [f32]) {
    if begin == 960 && end == 960 {
        return;
    }

    let gain = &TABLES.gain;

    if begin == end {
        let g = gain[end];
        for s in output.iter_mut() {
            *s *= g;
        }
    }
    else {
        let a = gain[begin] * (1.0 / (FRAME_SAMPLES - 1) as f32);
        let b = gain[end] * (1.0 / (FRAME_SAMPLES - 1) as f32);

        for (i, s) in output.iter_mut().enumerate() {
            *s *= a * (FRAME_SAMPLES - i - 1) as f32 + b * i as f32;
        }
    }
}

/// Dolby E decoder.
pub struct DolbyEDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<f32>,
    words: WordStream,
    frame: Frame,
    transform: Transform,
    /// The overlap of the last block of each channel.
    history: [[f32; OVERLAP]; MAX_CHANNELS],
    block: Box<[f32; BLOCK_LEN]>,
    multi_prog_warned: bool,
}

impl DolbyEDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_DOLBY_E {
            return unsupported_error("dolby_e: invalid codec");
        }

        let rate = params.sample_rate.unwrap_or(DEFAULT_RATE);
        let channels =
            params.channels.clone().unwrap_or_else(|| symphonia_common::mpeg::dolby_e::channels(6));

        Ok(DolbyEDecoder {
            params: params.clone(),
            opts: *opts,
            buf: AudioBuffer::new(AudioSpec::new(rate, channels), FRAME_SAMPLES),
            words: WordStream::new(),
            frame: Frame::new(),
            transform: Transform::new(),
            history: [[0.0; OVERLAP]; MAX_CHANNELS],
            block: Box::new([0.0; BLOCK_LEN]),
            multi_prog_warned: false,
        })
    }

    /// Transform the channel of a segment: output the samples of the segment, and keep the overlap
    /// with the next.
    fn transform_segment(
        transform: &mut Transform,
        block: &mut [f32; BLOCK_LEN],
        channel: &frame::Channel,
        history: &mut [f32; OVERLAP],
        output: &mut [f32],
    ) {
        transform.transform_groups(&channel.groups[..channel.nb_groups], &channel.mantissas, block);

        for i in 0..OVERLAP {
            output[i] = history[i] + block[i];
        }

        output[OVERLAP..SEGMENT_SAMPLES].copy_from_slice(&block[OVERLAP..SEGMENT_SAMPLES]);
        history.copy_from_slice(&block[SEGMENT_SAMPLES..SEGMENT_SAMPLES + OVERLAP]);
    }

    fn decode_packet(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        self.buf.clear();

        let data = packet.data;
        let header = DolbyEHeader::parse(&mut self.words, data)?;

        if header.nb_programs > 1 && !self.multi_prog_warned {
            warn!(
                "dolby_e: stream has {} programs (configuration {}), channels are output in native \
                 order",
                header.nb_programs, header.prog_conf
            );
            self.multi_prog_warned = true;
        }

        let half = header.nb_channels / 2;
        let all = header.nb_channels;

        let words = &mut self.words;
        self.frame.parse_audio(words, data, &header, 0, half, 0)?;
        self.frame.parse_audio(words, data, &header, half, all, 0)?;

        if header.mtd_ext_size != 0 {
            words.skip_words(
                usize::from(words.key_present()) + usize::from(header.mtd_ext_size) + 1,
            )?;
        }

        self.frame.parse_audio(words, data, &header, 0, half, 1)?;
        self.frame.parse_audio(words, data, &header, half, all, 1)?;

        if header.meter_size != 0 {
            words.skip_words(
                usize::from(words.key_present()) + usize::from(header.meter_size) + 1,
            )?;
        }

        // Re-create the buffer if the stream changed.
        let spec =
            AudioSpec::new(header.sample_rate, symphonia_common::mpeg::dolby_e::channels(all));

        if self.buf.spec() != &spec {
            self.buf = AudioBuffer::new(spec, FRAME_SAMPLES);
        }

        self.buf.render_uninit(Some(FRAME_SAMPLES));

        let order = channel_order(&header);

        for ch in 0..all {
            let Some(plane) = self.buf.plane_mut(usize::from(order[ch]))
            else {
                continue;
            };

            let (first, second) = plane.split_at_mut(SEGMENT_SAMPLES);

            for (seg, output) in [first, second].into_iter().enumerate() {
                Self::transform_segment(
                    &mut self.transform,
                    &mut self.block,
                    &self.frame.channels[seg][ch],
                    &mut self.history[ch],
                    output,
                );
            }

            apply_gain(usize::from(header.begin_gain[ch]), usize::from(header.end_gain[ch]), plane);
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for DolbyEDecoder {
    fn reset(&mut self) {
        self.history = [[0.0; OVERLAP]; MAX_CHANNELS];
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_packet(packet) {
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

impl RegisterableAudioDecoder for DolbyEDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(DolbyEDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_DOLBY_E, "dolby_e", "Dolby E")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::*;

    /// The mantissas of every group fill exactly the coefficients of its transform.
    #[test]
    fn verify_group_tables() {
        for seg in &FRM_OFS {
            for (gr_code, groups) in seg.iter().enumerate() {
                assert_eq!(groups.len(), usize::from(NB_GROUPS[gr_code]));

                for g in *groups {
                    let coefs = 1usize << (IMDCT_BITS[usize::from(g.imdct_idx)] - 1);
                    let total: usize = g.nb_mantissa[..usize::from(g.nb_exponent)]
                        .iter()
                        .map(|&n| usize::from(n))
                        .sum();

                    // A group codes fewer mantissas than coefficients only where the bandwidth
                    // code (up to 7 bands) or the LFE leaves the highest ones out.
                    assert!(total <= coefs);
                    assert!(usize::from(g.mnt_ofs) + coefs <= 1024);
                    assert!(usize::from(g.src_ofs) + usize::from(g.win_len) <= 2 * coefs);
                    assert!(usize::from(g.dst_ofs) + usize::from(g.win_len) <= BLOCK_LEN);
                    assert!(usize::from(g.win_ofs) + usize::from(g.win_len) <= 3712);
                }
            }
        }
    }
}
