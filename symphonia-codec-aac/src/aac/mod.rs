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
    Audio, AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels,
    GenericAudioBufferRef,
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
    /// HE-AAC v1 (SBR) runtime state, present once SBR is known active for this stream. See
    /// [`SbrRuntime`].
    sbr: Option<SbrRuntime>,
}

/// Per-stream SBR (HE-AAC v1) runtime state, present once SBR is known active: eagerly at
/// [`AacDecoder::try_new`] for explicit ASC signalling (`asc.sbr_present`), or lazily from
/// [`AacDecoder::decode_ga`] the first time a `fil_element()` carries an `EXT_SBR_DATA` /
/// `EXT_SBR_DATA_CRC` payload for implicit signalling (an ASC/ADTS header that declares plain
/// LC at the core rate). See `sbr::mod` for the ported SBR tool itself.
struct SbrRuntime {
    /// The SBR internal / output sample rate: `2 ×` the AAC core rate for the standard
    /// dual-rate mode. (`sbr::decoder::SbrDecoder`'s §4.6.18.4.3 downsampled-output mode is
    /// ported but not selected here — see `sbr::mod`'s doc comment.)
    fs_sbr: u32,
    /// One SBR decoder + header-reuse state per [`AacDecoder::elem_targets`] entry (SCE/CPE),
    /// in bitstream order.
    elems: Vec<SbrElemState>,
    /// Output buffer at `fs_sbr`, `2 ×` the core `AudioBuffer`'s capacity.
    buf: AudioBuffer<f32>,
}

struct SbrElemState {
    decoder: sbr::decoder::SbrDecoder,
    /// The most recently parsed `sbr_header()` for this element, threaded into
    /// [`sbr::extension::SbrExtensionData::parse`]'s `prev_header` parameter so a
    /// `bs_header_flag == 0` payload (header reuse) parses correctly.
    prev_header: Option<sbr::header::SbrHeader>,
}

impl SbrRuntime {
    fn new(
        fs_sbr: u32,
        channels: Channels,
        core_samples: usize,
        elem_targets: &[(bool, [usize; 2])],
    ) -> Result<Self> {
        let elems = elem_targets
            .iter()
            .map(|(is_pair, _)| {
                let decoder = sbr::decoder::SbrDecoder::new(fs_sbr, if *is_pair { 2 } else { 1 })?;
                Ok(SbrElemState { decoder, prev_header: None })
            })
            .collect::<Result<Vec<_>>>()?;

        let buf = AudioBuffer::new(AudioSpec::new(fs_sbr, channels), core_samples * 2);

        Ok(SbrRuntime { fs_sbr, elems, buf })
    }
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

        // Check complexity. HE-AAC v1 (SBR) is supported: `asc.sbr_present` is set, with
        // `asc.object_type` already rewound to the inner base AOT (checked below) by the
        // hierarchical explicit-signalling branch of `AudioSpecificConfig::read`. HE-AAC v2
        // (Parametric Stereo, `asc.ps_present`) is not — see NOTICE.
        if asc.ps_present {
            return unsupported_error("aac: parametric stereo (HE-AAC v2) is not yet supported");
        }
        if asc.object_type != AudioObjectType::Lc || asc.samples != 1024 {
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

        // Explicit SBR signalling (ASC hierarchical `audioObjectType == 5`/`29` wrapper) gives
        // the SBR/output sample rate up front (ISO/IEC 14496-3 §1.6.3.8:
        // `extensionSamplingFrequencyIndex` is "the output sampling frequency"), so the
        // decoder can advertise the doubled rate immediately and callers that resolve the
        // output sample rate once at open time (e.g. rmpd-player's `AudioDecoderState`) see
        // the correct value without special-casing SBR. *Implicit* signalling (an ASC/ADTS
        // header that declares plain LC at the core rate, with SBR only discovered once
        // `fil_element()`s start arriving — see `decode_ga`) cannot do this: `self.sbr` stays
        // `None` here and the doubled rate only becomes visible via the first SBR-bearing
        // `AudioBuffer`'s own `AudioSpec` (`decode_ref`/`last_decoded`) — callers that only
        // read `codec_params().sample_rate` once at open time need a follow-up fix to also
        // consult the first decoded buffer's spec, mirroring how they already do for channel
        // count.
        let sbr = if asc.sbr_present {
            let fs_sbr = asc
                .sbr_ps_info
                .as_ref()
                .map(|(rate, _)| *rate)
                .unwrap_or_else(|| asc.sample_rate.saturating_mul(2));
            Some(SbrRuntime::new(fs_sbr, channels.clone(), asc.samples, &elem_targets)?)
        }
        else {
            None
        };

        params.with_channels(channels.clone()).with_sample_rate(sbr.as_ref().map_or(asc.sample_rate, |s| s.fs_sbr));

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
            sbr,
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

    fn decode_ga<B: ReadBitsLtr + FiniteBitStream>(&mut self, bs: &mut B, data: &[u8]) -> Result<()> {
        let mut cur_pair = 0;
        let mut sbr_ext: Vec<Option<sbr::extension::SbrExtensionData>> =
            vec![None; self.elem_targets.len()];
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

                        if matches!(ext_type, 0xd | 0xe) && cur_pair > 0 {
                            // EXT_SBR_DATA (0xd) / EXT_SBR_DATA_CRC (0xe). `sbr_extension_data()`
                            // starts immediately after this 4-bit `extension_type` field (no
                            // byte alignment) and runs to the end of this `count`-byte
                            // `extension_payload()`, extending the most recently decoded
                            // SCE/CPE (`cur_pair - 1`). The ported SBR parser
                            // (`sbr::extension`) needs its own `sbr::bits::BitReader` over
                            // `data`; splice it in at the outer reader's current bit position,
                            // then fast-forward the outer reader by however many bits the SBR
                            // parse consumed so the rest of this loop (fill/TERM detection)
                            // stays in sync.
                            let elem_idx = cur_pair - 1;
                            let (is_pair, _) = self.elem_targets[elem_idx];
                            let crc_flag = ext_type == 0xe;
                            let id_aac =
                                if is_pair { sbr::IdSynEle::Cpe } else { sbr::IdSynEle::Sce };

                            let payload_start = (data.len() as u64) * 8 - bs.bits_left();

                            if self.sbr.is_none() {
                                // Implicit signalling: the ASC/ADTS header declared plain LC at
                                // the core rate, and this is the first EXT_SBR_DATA payload
                                // seen. Assume the standard 2× dual-rate ratio (no
                                // `extensionSamplingFrequencyIndex` is available outside the
                                // ASC); `self.params.sample_rate` is corrected below so any
                                // caller that re-reads `codec_params()` after this point sees
                                // the doubled rate — but see the crate README for callers that
                                // only read it once at open time.
                                let fs_sbr = self.asc.sample_rate.saturating_mul(2);
                                self.sbr = Some(SbrRuntime::new(
                                    fs_sbr,
                                    self.buf.spec().channels().clone(),
                                    self.asc.samples,
                                    &self.elem_targets,
                                )?);
                                self.params.with_sample_rate(fs_sbr);
                            }
                            let sbr = self.sbr.as_mut().expect("just constructed above");
                            let prev_header = sbr.elems[elem_idx].prev_header;

                            let mut sbr_bs = sbr::bits::BitReader::new(data);
                            sbr_bs.ignore_bits(u32::try_from(payload_start).unwrap_or(u32::MAX))?;
                            let ext = sbr::extension::SbrExtensionData::parse(
                                &mut sbr_bs,
                                id_aac,
                                crc_flag,
                                sbr.fs_sbr,
                                Some(count as u32),
                                prev_header,
                            )?;
                            let consumed = sbr_bs.bit_position() - payload_start;
                            bs.ignore_bits(u32::try_from(consumed).unwrap_or(0))?;

                            sbr.elems[elem_idx].prev_header = Some(ext.header);
                            sbr_ext[elem_idx] = Some(ext);
                        }
                        else {
                            // Ignore extension payload(s).
                            bs.ignore_bits(4)?;
                            for _ in 0..count - 1 {
                                bs.ignore_bits(8)?;
                            }
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

        // SBR reconstruction: once active for the stream, every element gets 2×-rate output
        // every frame — `process_frame()` when this frame carried a payload,
        // `upsample_frame()` (§4.6.18.5 pure upsampling) otherwise, so the QMF state and
        // output rate stay continuous even across headerless / SBR-silent frames.
        if self.sbr.is_some() {
            // Gather the core PCM for each element first, before taking a mutable borrow of
            // `self.sbr`. Symphonia's `AudioBuffer<f32>` samples are normalized to `[-1.0,
            // 1.0]`, but the ported SBR tool's envelope-energy formulas (`EOrig = 64 *
            // 2^(E/a)`, `QOrig`, and every energy comparison the envelope adjuster/limiter
            // performs against them) are unchanged from oxideav-aac, whose own core decoder
            // keeps samples in the full-scale 16-bit PCM domain (`[-32768, 32768]`) right up
            // to the final `pcm::interleave_s16` rounding step -- that is the domain those
            // formulas are calibrated against (ISO/IEC 14496-3 §4.6.18.3.5's constants assume
            // it implicitly, as the "PCM" the whole tool was specified around). Rescale by
            // `32768` going in so the analysis QMF / HF generation / envelope adjustment see
            // energies on the scale the transmitted envelope/noise values target; the
            // symmetric `/ 32768` on the way out (below) restores Symphonia's normalized
            // range. Without this, the low band (a raw, ungained `XLow` passthrough) stays at
            // Symphonia's `~1.0` scale while the high band is forced to the transmitted
            // envelope's absolute `~32768` scale by the adjuster's gain match -- a 2^30
            // energy-scale mismatch between the two halves of the same reconstructed signal.
            const PCM_SCALE: f64 = 32768.0;
            let mut core_pcm: Vec<Vec<Vec<f64>>> = Vec::with_capacity(cur_pair);
            for (is_pair, indices) in self.elem_targets.iter().take(cur_pair) {
                let n_ch = if *is_pair { 2 } else { 1 };
                let mut chans = Vec::with_capacity(n_ch);
                for &ch_idx in indices.iter().take(n_ch) {
                    let plane = self.buf.plane(ch_idx).expect("core channel plane exists");
                    chans.push(
                        plane.iter().map(|&s| f64::from(s) * PCM_SCALE).collect::<Vec<f64>>(),
                    );
                }
                core_pcm.push(chans);
            }

            let sbr = self.sbr.as_mut().expect("checked is_some above");
            sbr.buf.clear();
            sbr.buf.render_uninit(None);

            for (idx, (_, indices)) in self.elem_targets.iter().take(cur_pair).enumerate() {
                let core_refs: Vec<&[f64]> = core_pcm[idx].iter().map(Vec::as_slice).collect();
                let elem = &mut sbr.elems[idx];
                let out = match &sbr_ext[idx] {
                    Some(ext) => elem.decoder.process_frame(ext, &core_refs)?,
                    None => elem.decoder.upsample_frame(&core_refs)?,
                };
                for (c, &ch_idx) in indices.iter().take(out.len()).enumerate() {
                    let dst = sbr.buf.plane_mut(ch_idx).expect("sbr channel plane exists");
                    let n = dst.len().min(out[c].len());
                    for (i, dst_i) in dst.iter_mut().enumerate().take(n) {
                        *dst_i = (out[c][i] / PCM_SCALE) as f32;
                    }
                }
            }
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
            AudioObjectType::Lc => self.decode_ga(&mut bs, packet.data)?,
            _ => return unsupported_error("aac: object type"),
        }

        // Trim gaps. `packet.trim_start`/`trim_end` are computed by the format reader
        // against the container's own declared sample rate (e.g. an MP4 track's `mdhd`/
        // `stsd` rate), which for an explicitly-signalled HE-AAC file is already the SBR
        // *output* rate (verified against a real Fraunhofer HE-AAC v1 fixture: `trim_start`
        // summed across the leading packets matches the MP4 edit-list encoder delay exactly
        // at the 44.1 kHz *output* rate, not the 22.05 kHz core rate) — so no rescaling is
        // needed for `sbr.buf`. This has only been validated against explicit ASC signalling
        // in MP4; ADTS (no container-level trim) and implicit signalling are unverified —
        // see the crate README.
        if self.opts.gapless {
            if let Some(sbr) = self.sbr.as_mut() {
                sbr.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
            }
            else {
                self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
            }
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
            if let Some(sbr) = self.sbr.as_mut() {
                sbr.buf.clear();
            }
            Err(e)
        }
        else if let Some(sbr) = &self.sbr {
            Ok(sbr.buf.as_generic_audio_buffer_ref())
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        match &self.sbr {
            Some(sbr) => sbr.buf.as_generic_audio_buffer_ref(),
            None => self.buf.as_generic_audio_buffer_ref(),
        }
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
