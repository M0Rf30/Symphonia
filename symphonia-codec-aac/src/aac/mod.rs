// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia_core::codecs::audio::{AudioDecoder, FinalizeResult};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::{codec_profile, support_audio_codec};

use symphonia_common::mpeg::audio::{
    AudioObjectType, AudioSpecificConfig, ChannelElement, channel_elements_for_config,
};

mod codebooks;
mod common;
mod cpe;
mod dsp;
mod ics;
mod sbr;
mod window;

use common::*;

/// Advanced Audio Coding (AAC) decoder.
///
/// Implements a decoder for Advanced Audio Decoding Low-Complexity (AAC-LC) as defined in
/// ISO/IEC 13818-7 and ISO/IEC 14496-3.
pub struct AacDecoder {
    // info: NACodecInfoRef,
    asc: AudioSpecificConfig,
    pairs: Vec<cpe::ChannelPair>,
    /// For each syntactic element (`SCE`/`CPE`/`LFE`) expected, in bitstream order, whether it is
    /// a channel pair, and the target output buffer channel index/indices. Built once from
    /// [`AudioSpecificConfig::channel_elements`] so that e.g. a 5.1 stream's `SCE(center)`,
    /// `CPE(front L/R)`, `CPE(rear L/R)`, `LFE` element order is placed into the buffer's
    /// ascending-channel-position order (FL, FR, FC, LFE, RL, RR).
    elem_targets: Vec<(bool, [usize; 2])>,
    dsp: dsp::Dsp,
    sbinfo: GASubbandInfo,
    params: AudioCodecParameters,
    buf: AudioBuffer<f32>,
    opts: AudioDecoderOptions,
}

/// Resolve each expected syntactic element (in bitstream order) into an output buffer channel
/// index/indices, using the canonical (ascending channel-position) buffer ordering that
/// [`Channels::Positioned`] implies.
fn build_elem_targets(
    elements: &[ChannelElement],
    channels: &Channels,
) -> Result<Vec<(bool, [usize; 2])>> {
    elements
        .iter()
        .map(|elem| {
            let index_of = |pos| {
                channels.get_canonical_index_for_positioned_channel(pos).ok_or(
                    symphonia_core::errors::Error::DecodeError(
                        "aac: channel position is not present in the channel set",
                    ),
                )
            };
            match *elem {
                ChannelElement::Single(pos) => Ok((false, [index_of(pos)?, 0])),
                ChannelElement::Pair(l, r) => Ok((true, [index_of(l)?, index_of(r)?])),
            }
        })
        .collect()
}

impl AacDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        // This decoder only supports AAC.
        if params.codec != CODEC_ID_AAC {
            return unsupported_error("aac: invalid codec");
        }

        // If extra data present, parse the audio specific config
        let asc = if let Some(extra_data_buf) = &params.extra_data {
            validate!(extra_data_buf.len() >= 2);
            AudioSpecificConfig::read(extra_data_buf)?
        }
        else {
            // Otherwise, assume there is no ASC and use the codec parameters for ADTS.
            let mut asc = AudioSpecificConfig::default();

            asc.object_type = AudioObjectType::Lc;
            asc.samples = 1024;

            asc.sample_rate = match params.sample_rate {
                Some(rate) => rate,
                None => return unsupported_error("aac: sample rate is required"),
            };

            asc.channels = params.channels.clone();
            asc.channel_elements = asc.channels.as_ref().and_then(channel_elements_for_config);

            asc
        };

        // The channel configuration must be known, either via the predefined
        // `channelConfiguration` values 1-7, or an explicit `program_config_element()`
        // (`channelConfiguration == 0`), both of which are resolved into `asc.channels` /
        // `asc.channel_elements` by [`AudioSpecificConfig::read`].
        let channels = match &asc.channels {
            Some(channels) => channels.clone(),
            _ => return unsupported_error("aac: channels or channel layout is required"),
        };

        // Check complexity.
        if asc.object_type != AudioObjectType::Lc || asc.sbr_present || asc.samples != 1024 {
            return unsupported_error("aac: aac too complex");
        }

        // Map each expected syntactic element (`SCE`/`CPE`/`LFE`), in bitstream order, onto its
        // output buffer channel index/indices (see the `elem_targets` field documentation).
        let elem_targets = match &asc.channel_elements {
            Some(elements) => build_elem_targets(elements, &channels)?,
            None => {
                return unsupported_error(
                    "aac: channel layout requires a channelConfiguration or program_config_element",
                );
            }
        };

        // Clone and amend the codec parameters with information from the extra data.
        let mut params = params.clone();

        params.with_channels(channels.clone()).with_sample_rate(asc.sample_rate);

        let sbinfo = GASubbandInfo::find(asc.sample_rate);

        let buf = AudioBuffer::new(AudioSpec::new(asc.sample_rate, channels), asc.samples);

        Ok(AacDecoder {
            asc,
            pairs: Vec::new(),
            elem_targets,
            dsp: dsp::Dsp::new(),
            sbinfo,
            params,
            buf,
            opts: *opts,
        })
    }

    fn set_pair(&mut self, pair_no: usize, channel: usize, pair: bool) -> Result<()> {
        if self.pairs.len() <= pair_no {
            self.pairs.push(cpe::ChannelPair::new(pair, channel, self.sbinfo));
        }
        else {
            validate!(self.pairs[pair_no].channel == channel);
            validate!(self.pairs[pair_no].is_pair == pair);
        }

        let max_channels = self.asc.channels.as_ref().map_or(0, |channels| channels.count());
        validate!(if pair { channel + 1 } else { channel } < max_channels);

        Ok(())
    }

    fn decode_ga<B: ReadBitsLtr + FiniteBitStream>(&mut self, bs: &mut B) -> Result<()> {
        let mut cur_pair = 0;
        while bs.bits_left() > 3 {
            let id = bs.read_bits_leq32(3)?;

            match id {
                0 | 3 => {
                    // ID_SCE / ID_LFE: both are single-channel elements; the target output
                    // channel (e.g. LFE1 vs. front-center) was already resolved into
                    // `elem_targets` from the channel configuration / PCE.
                    let _tag = bs.read_bits_leq32(4)?;
                    validate!(cur_pair < self.elem_targets.len());
                    let (is_pair, indices) = self.elem_targets[cur_pair];
                    validate!(!is_pair);
                    self.set_pair(cur_pair, indices[0], false)?;
                    self.pairs[cur_pair].decode_ga_sce(bs, self.asc.object_type)?;
                    cur_pair += 1;
                }
                1 => {
                    // ID_CPE
                    let _tag = bs.read_bits_leq32(4)?;
                    validate!(cur_pair < self.elem_targets.len());
                    let (is_pair, indices) = self.elem_targets[cur_pair];
                    validate!(is_pair);
                    self.set_pair(cur_pair, indices[0], true)?;
                    self.pairs[cur_pair].decode_ga_cpe(bs, self.asc.object_type)?;
                    cur_pair += 1;
                }
                2 => {
                    // ID_CCE (coupling channel element): used for encoder-side downmix/dialogue
                    // normalization hints. Vanishingly rare outside broadcast encoders; no
                    // bitstream in the validation corpus (ffmpeg output, Fraunhofer conformance
                    // samples) produces one, so it is left unsupported rather than applying an
                    // unverified gain.
                    return unsupported_error("aac: coupling channel element");
                }
                4 => {
                    // ID_DSE
                    let _id = bs.read_bits_leq32(4)?;
                    let align = bs.read_bool()?;
                    let mut count = bs.read_bits_leq32(8)?;
                    if count == 255 {
                        count += bs.read_bits_leq32(8)?;
                    }
                    if align {
                        bs.realign(); // ????
                    }
                    bs.ignore_bits(count * 8)?; // no SBR payload or such
                }
                5 => {
                    // ID_PCE appearing inside raw_data_block(), as opposed to inside the
                    // AudioSpecificConfig's GASpecificConfig() (which IS parsed and honoured; see
                    // `AudioSpecificConfig::read` / `channel_elements`). This in-band form is used
                    // when the transport carries no out-of-band channel configuration: MP4/ESDS
                    // always carries an ASC (so this is unreachable there), but ADTS's own
                    // `channel_configuration` field can itself be 0, in which case a `raw_data_
                    // block()` is required to open with exactly this element. Supporting it would
                    // mean deferring the decoder's channel count / output buffer sizing (currently
                    // fixed at construction from `AudioSpecificConfig`/ADTS header) until the
                    // first packet has been parsed; a real (if rare) case, but out of scope here.
                    return unsupported_error("aac: program config element in raw_data_block");
                }
                6 => {
                    // ID_FIL
                    let mut count = bs.read_bits_leq32(4)? as usize;
                    if count == 15 {
                        count += bs.read_bits_leq32(8)? as usize;
                        count -= 1;
                    }

                    // Check if the ID_FIL element contains SBR data. Note that ID_FIL elements with
                    // SBR data may not contain other extension payloads.
                    if count > 0 {
                        let ext_type = bs.read_bits_leq32(4)?;

                        match ext_type {
                            // EXT_SBR_DATA (0xd)
                            // EXT_SBR_DATA_CRC (0xe)
                            0xd | 0xe => self.asc.sbr_present = true,
                            // EXT_FILL (0x0)
                            // EXT_FILL_DATA (0x1)
                            // EXT_DATA_ELEMENT (0x2)
                            // EXT_DYNAMIC_RANGE (0xb)
                            // EXT_SAC_DATA (0xc)
                            _ => (),
                        }

                        // Ignore extension payload(s).
                        bs.ignore_bits(4)?;
                        for _ in 0..count - 1 {
                            bs.ignore_bits(8)?;
                        }
                    }
                }
                7 => {
                    // ID_TERM
                    break;
                }
                _ => unreachable!(),
            };
        }
        let rate_idx = GASubbandInfo::find_idx(self.asc.sample_rate);
        for pair in 0..cur_pair {
            self.pairs[pair].synth_audio(&mut self.dsp, &mut self.buf, rate_idx);
        }
        Ok(())
    }

    // fn flush(&mut self) {
    //     for pair in self.pairs.iter_mut() {
    //         pair.ics[0].delay = [0.0; 1024];
    //         pair.ics[1].delay = [0.0; 1024];
    //     }
    // }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // Clear the audio output buffer.
        self.buf.clear();
        self.buf.render_uninit(None);

        let mut bs = BitReaderLtr::new(packet.data);

        // Choose decode step based on the object type.
        match self.asc.object_type {
            AudioObjectType::Lc => self.decode_ga(&mut bs)?,
            _ => return unsupported_error("aac: object type"),
        }

        // Trim gaps.
        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for AacDecoder {
    fn reset(&mut self) {
        for pair in self.pairs.iter_mut() {
            pair.reset();
        }
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

impl RegisterableAudioDecoder for AacDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AacDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        use symphonia_core::codecs::audio::well_known::profiles::CODEC_PROFILE_AAC_LC;

        &[support_audio_codec!(
            CODEC_ID_AAC,
            "aac",
            "Advanced Audio Coding",
            &[codec_profile!(CODEC_PROFILE_AAC_LC, "aac-lc", "Low Complexity"),]
        )]
    }
}
