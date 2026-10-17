// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Unified Speech and Audio Coding (USAC, xHE-AAC, audio object type 42) decoder.
//!
//! The frequency-domain core coder (`fd_channel_stream()`, ISO/IEC 23003-3 7.3) is implemented;
//! the linear-prediction-domain coder, enhanced SBR and MPEG Surround 2-1-2 are owned by sibling
//! workstreams and are rejected here until they land.

mod arith;
mod esbr;
mod fd;
mod lpd;
mod mps212;
mod stereo;
mod tables;
mod tns;

use symphonia_core::audio::{AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia_core::codecs::audio::{AudioDecoder, FinalizeResult};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::FiniteBitStream;
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::{codec_profile, support_audio_codec};

use symphonia_common::mpeg::audio::usac::{
    UsacConfig, UsacElementConfig, UsacExtElementConfig, UsacExtType, UsacSbrConfig,
    read_escaped_value,
};
use symphonia_common::mpeg::audio::{AudioObjectType, AudioSpecificConfig};

use super::build_elem_targets;

use esbr::EsbrDecoder;
use fd::{FdChannelState, FdFilterbank};
use lpd::LpdDecoder;
use stereo::StereoState;

/// The scale of the internal samples, `1 / I16_SCALE`, applied when the core samples are written
/// into the output buffer.
const PCM_SCALE: f32 = symphonia_core::audio::conv::I16_SCALE;

/// The noise-filling seed of the first channel of a syntactic element (ISO/IEC 23003-3 7.5.1.4).
const NOISE_SEED_FIRST: u32 = 0x3039;
/// The noise-filling seed of the second channel of a channel pair.
const NOISE_SEED_SECOND: u32 = 0x10932;

/// The sampling-frequency index of the USAC scale factor band tables (ISO/IEC 23003-3 Tables 85
/// to 90): 13 entries, index 0 for 96 kHz and index 12 for 7.35 kHz.
fn usac_sampling_index(core_rate: u32) -> usize {
    const THRESHOLDS: [u32; 12] =
        [92017, 75132, 55426, 46009, 37566, 27713, 23004, 18783, 13856, 11502, 9391, 7675];

    THRESHOLDS.iter().position(|&t| core_rate >= t).unwrap_or(12)
}

/// A single channel element (also used for an LFE element).
struct SceState {
    target: usize,
    ch: FdChannelState,
    pcm: Vec<f32>,
    sbr: Option<EsbrDecoder>,
    lpd: LpdDecoder,
    /// Whether the previous frame was linear-prediction-domain coded.
    last_lpd: Option<bool>,
}

/// A channel pair element.
struct CpeState {
    targets: [usize; 2],
    ch0: FdChannelState,
    ch1: FdChannelState,
    stereo: StereoState,
    pcm0: Vec<f32>,
    pcm1: Vec<f32>,
    stereo_config_index: u8,
    sbr: Option<EsbrDecoder>,
}

/// The state of one syntactic element (SCE, CPE or LFE).
#[allow(clippy::large_enum_variant)]
enum Element {
    Sce(SceState),
    Cpe(CpeState),
}

/// The USAC decoder.
pub struct UsacDecoder {
    config: UsacConfig,
    core_frame_len: usize,
    sampling_index: usize,
    filterbank: FdFilterbank,
    elements: Vec<Element>,
    params: AudioCodecParameters,
    buf: AudioBuffer<f32>,
    opts: AudioDecoderOptions,
    /// The access unit with zero padding, which the arithmetic decoder reads past the end of.
    frame_buf: Vec<u8>,
    /// Whether an access unit has been decoded since construction or the last reset. Pre-roll
    /// units only prime a decoder without history; replaying them in a running decoder would
    /// corrupt its overlap and context state.
    primed: bool,
}

/// The number of zero bits appended to an access unit. The arithmetic decoder reads 16 bits to
/// start and then gives 14 back (ISO/IEC 23003-3 7.4), so it may read past the end of the data
/// of a short channel stream.
pub(super) const PAD_BITS: usize = 64;

impl UsacDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_AAC {
            return unsupported_error("aac: invalid codec");
        }

        // The USAC configuration is signalled through the audio specific config.
        let asc = match &params.extra_data {
            Some(extra_data) => AudioSpecificConfig::read(extra_data)?,
            None => return unsupported_error("aac: usac requires an audio specific config"),
        };

        if asc.object_type != AudioObjectType::Usac {
            return unsupported_error("aac: not a usac stream");
        }

        let config = match &asc.usac {
            Some(config) => config.clone(),
            None => return decode_error("aac: usac stream without a usac config"),
        };

        // The linear-prediction-domain and MPS212 tools are not implemented yet; reject
        // configurations that require them up front.
        for element in &config.elements {
            if let UsacElementConfig::Cpe { stereo_config_index, .. } = element {
                if *stereo_config_index > 0 {
                    return unsupported_error("aac: usac mps212 is not yet implemented");
                }
            }
        }

        let core_frame_len = config.core_frame_length;
        let core_rate = config.core_sampling_frequency();
        let sampling_index = usac_sampling_index(core_rate);

        let filterbank = FdFilterbank::new(core_frame_len);

        // Resolve each expected syntactic element (in bitstream order) to its output channel
        // indices, using the canonical ascending channel-position order.
        let elem_targets = build_elem_targets(&config.channel_elements, &config.channels)?;

        let mut elements = Vec::with_capacity(elem_targets.len());
        let mut target_iter = elem_targets.into_iter();

        for element in &config.elements {
            let element = match element {
                UsacElementConfig::Sce { sbr, .. } => {
                    let (is_pair, targets) = target_iter.next().expect("element targets align");
                    debug_assert!(!is_pair);
                    let sbr = Self::make_sbr(&config, sbr, 1, core_frame_len)?;
                    Element::Sce(SceState {
                        target: targets[0],
                        ch: FdChannelState::new(core_frame_len, sampling_index, NOISE_SEED_FIRST),
                        pcm: vec![0.0; core_frame_len],
                        sbr,
                        lpd: LpdDecoder::new(core_frame_len, core_rate),
                        last_lpd: None,
                    })
                }
                UsacElementConfig::Cpe { sbr, stereo_config_index, .. } => {
                    let (is_pair, targets) = target_iter.next().expect("element targets align");
                    debug_assert!(is_pair);
                    let sbr = Self::make_sbr(&config, sbr, 2, core_frame_len)?;
                    Element::Cpe(CpeState {
                        targets,
                        ch0: FdChannelState::new(core_frame_len, sampling_index, NOISE_SEED_FIRST),
                        ch1: FdChannelState::new(core_frame_len, sampling_index, NOISE_SEED_SECOND),
                        stereo: StereoState::new(core_frame_len),
                        pcm0: vec![0.0; core_frame_len],
                        pcm1: vec![0.0; core_frame_len],
                        stereo_config_index: *stereo_config_index,
                        sbr,
                    })
                }
                UsacElementConfig::Lfe => {
                    let (is_pair, targets) = target_iter.next().expect("element targets align");
                    debug_assert!(!is_pair);
                    Element::Sce(SceState {
                        target: targets[0],
                        ch: FdChannelState::new(core_frame_len, sampling_index, NOISE_SEED_FIRST),
                        pcm: vec![0.0; core_frame_len],
                        sbr: None,
                        lpd: LpdDecoder::new(core_frame_len, core_rate),
                        last_lpd: None,
                    })
                }
                UsacElementConfig::Ext(_) => continue,
            };

            elements.push(element);
        }

        let mut params = params.clone();
        params.with_channels(config.channels.clone()).with_sample_rate(config.sampling_frequency);

        let buf = AudioBuffer::new(
            AudioSpec::new(config.sampling_frequency, config.channels.clone()),
            config.output_frame_length(),
        );

        Ok(UsacDecoder {
            config,
            core_frame_len,
            sampling_index,
            filterbank,
            elements,
            primed: false,
            params,
            buf,
            opts: *opts,
            frame_buf: Vec::new(),
        })
    }

    /// The eSBR decoder of an element, if its configuration has one.
    fn make_sbr(
        config: &UsacConfig,
        sbr: &Option<UsacSbrConfig>,
        channels: usize,
        core_frame_len: usize,
    ) -> Result<Option<EsbrDecoder>> {
        match sbr {
            Some(cfg) => Ok(Some(EsbrDecoder::new(
                *cfg,
                config.sbr_ratio,
                channels,
                config.sampling_frequency,
                core_frame_len,
            )?)),
            None => Ok(None),
        }
    }

    /// Decode one USAC access unit into the internal output buffer.
    fn decode_frame(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let mut data = std::mem::take(&mut self.frame_buf);
        data.clear();
        data.extend_from_slice(packet.data);
        data.resize(packet.data.len() + PAD_BITS / 8, 0);

        let result = self.decode_frame_data(packet, &data);
        self.frame_buf = data;
        result
    }

    fn decode_frame_data(&mut self, packet: &PacketRef<'_>, data: &[u8]) -> Result<()> {
        self.decode_access_unit(data, false)?;

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }

    /// Decode one access unit into `self.buf`. `data` carries `PAD_BITS` of zero padding. A
    /// `priming` access unit is a pre-roll one: it is decoded to prime the decoder state and its
    /// own pre-roll payload is not interpreted.
    fn decode_access_unit(&mut self, data: &[u8], priming: bool) -> Result<()> {
        self.buf.clear();
        self.buf.render_uninit(None);

        let mut bs = BitReaderLtr::new(data);

        // usacIndependencyFlag (ISO/IEC 23003-3 6.2).
        let indep_flag = bs.read_bool()?;

        let mut elem_idx = 0;

        for i in 0..self.config.elements.len() {
            let cfg = &self.config.elements[i];

            match cfg {
                UsacElementConfig::Sce { .. } | UsacElementConfig::Lfe => {
                    let noise_fill = match cfg {
                        UsacElementConfig::Sce { core, .. } => core.noise_filling,
                        _ => false,
                    };
                    let tw_mdct = match cfg {
                        UsacElementConfig::Sce { core, .. } => core.tw_mdct,
                        _ => false,
                    };

                    if tw_mdct {
                        return unsupported_error("aac: usac time warped mdct");
                    }

                    self.decode_sce(elem_idx, noise_fill, &mut bs, data, indep_flag)?;
                    elem_idx += 1;
                }
                UsacElementConfig::Cpe { core, stereo_config_index, .. } => {
                    if core.tw_mdct {
                        return unsupported_error("aac: usac time warped mdct");
                    }

                    if *stereo_config_index > 0 {
                        return unsupported_error("aac: usac mps212 is not yet implemented");
                    }

                    self.decode_cpe(elem_idx, core.noise_filling, &mut bs, data, indep_flag)?;
                    elem_idx += 1;
                }
                UsacElementConfig::Ext(ext) => {
                    let payload = Self::read_ext_element(ext, &mut bs)?;

                    if ext.ext_type == UsacExtType::AudioPreroll && !priming && !self.primed {
                        if let Some(payload) = payload {
                            self.decode_preroll(&payload)?;
                        }
                    }
                }
            }
        }

        // A stream that reads into the padding is truncated.
        if bs.bits_left() < PAD_BITS as u64 {
            return decode_error("aac: usac access unit is truncated");
        }

        self.primed = true;

        Ok(())
    }

    /// Decode the pre-roll access units of an `AudioPreRoll()` payload (ISO/IEC 23003-3 Table
    /// 28) to prime the overlap and context state. Their output is discarded; the decoder
    /// configuration carried in the payload is assumed to match the stream's.
    fn decode_preroll(&mut self, payload: &[u8]) -> Result<()> {
        let mut bs = BitReaderLtr::new(payload);

        let config_len = read_escaped_value(&mut bs, 4, 4, 8)?;
        bs.ignore_bits(config_len * 8)?;

        // applyCrossfade, reserved.
        bs.ignore_bits(2)?;

        let num_au = read_escaped_value(&mut bs, 2, 4, 0)?;

        let mut au = Vec::new();

        for _ in 0..num_au {
            let au_len = read_escaped_value(&mut bs, 16, 16, 0)? as usize;

            if bs.bits_left() < au_len as u64 * 8 {
                return decode_error("aac: usac pre-roll access unit is truncated");
            }

            au.clear();
            for _ in 0..au_len {
                au.push(bs.read_bits_leq32(8)? as u8);
            }
            au.resize(au_len + PAD_BITS / 8, 0);

            // A pre-roll access unit that cannot be decoded only costs the priming.
            let _ = self.decode_access_unit(&au, true);
        }

        Ok(())
    }

    /// Decode a single channel element (`fd_channel_stream()`, ISO/IEC 23003-3 7.3).
    fn decode_sce(
        &mut self,
        elem_idx: usize,
        noise_fill: bool,
        bs: &mut BitReaderLtr<'_>,
        data: &[u8],
        indep_flag: bool,
    ) -> Result<()> {
        let lpd_frame = bs.read_bool()?;

        let Element::Sce(state) = &mut self.elements[elem_idx]
        else {
            return decode_error("aac: usac element mismatch");
        };

        if state.last_lpd.is_some_and(|last| last != lpd_frame) {
            return unsupported_error("aac: usac fd/lpd transitions are not yet implemented");
        }

        if lpd_frame {
            state.lpd.decode(
                bs,
                &mut state.ch.arith,
                &mut state.ch.noise_seed,
                indep_flag,
                &mut state.pcm,
            )?;
            state.last_lpd = Some(true);

            if let Some(sbr) = &mut state.sbr {
                sbr.parse(bs, data, indep_flag)?;
            }

            return Self::output_sce(&mut self.buf, state);
        }

        state.last_lpd = Some(false);

        let tns_data_present = bs.read_bool()?;

        state.ch.parse(bs, indep_flag, noise_fill, tns_data_present, None)?;

        if let Some(sbr) = &mut state.sbr {
            sbr.parse(bs, data, indep_flag)?;
        }

        // TNS is applied to a single channel after the stereo tools (a no-op here).
        let ics = state.ch.ics.clone();
        state.ch.tns.synth(
            ics.swb_offset,
            ics.max_sfb,
            ics.tns_max_bands,
            ics.num_windows,
            ics.window_len(self.core_frame_len),
            &mut state.ch.coeffs,
        );

        self.filterbank.synth(&mut state.ch, &mut state.pcm);

        Self::output_sce(&mut self.buf, state)
    }

    /// Write the core samples of a single channel element to its output plane, through eSBR if
    /// the stream uses it.
    fn output_sce(buf: &mut AudioBuffer<f32>, state: &mut SceState) -> Result<()> {
        let target = state.target;
        let scale = 1.0 / PCM_SCALE;
        let Some(plane) = buf.plane_mut(target)
        else {
            return decode_error("aac: invalid channel");
        };

        if let Some(sbr) = &mut state.sbr {
            return sbr.process(&[&state.pcm], &mut [plane], scale);
        }

        for (dst, src) in plane.iter_mut().zip(&state.pcm) {
            *dst = src * scale;
        }

        Ok(())
    }

    /// Decode a channel pair element.
    fn decode_cpe(
        &mut self,
        elem_idx: usize,
        noise_fill: bool,
        bs: &mut BitReaderLtr<'_>,
        data: &[u8],
        indep_flag: bool,
    ) -> Result<()> {
        let core_mode0 = bs.read_bool()?;
        let core_mode1 = bs.read_bool()?;

        if core_mode0 || core_mode1 {
            return unsupported_error("aac: usac lpd is not yet implemented");
        }

        let Element::Cpe(state) = &mut self.elements[elem_idx]
        else {
            return decode_error("aac: usac element mismatch");
        };

        let stereo = &mut state.stereo;

        // Zero the current-frame alpha values.
        stereo.alpha_zero_current();

        let tns_active = bs.read_bool()?;
        let common_window = bs.read_bool()?;
        stereo.common_window = common_window;

        if !common_window || indep_flag {
            stereo.reset_prev_alphas();
        }

        let ccfl = self.core_frame_len;
        let sampling_index = self.sampling_index;

        let (mut tns_data_present0, mut tns_data_present1) = (false, false);
        let mut tns_on_lr = false;
        let mut shared_tns = None;

        if common_window {
            // Read the shared ics_info().
            let mut ics = state.ch0.ics.clone();
            ics.read_window_header(bs)?;
            ics.read_max_sfb(bs)?;
            ics.setup(ccfl, sampling_index)?;

            let mut ics2 = ics.clone();
            if !bs.read_bool()? {
                // common_max_sfb == 0: the second channel has its own max_sfb.
                ics2.read_max_sfb_only(bs)?;
                ics2.setup(ccfl, sampling_index)?;
            }

            // A change in the transform sequence resets the previous-frame prediction.
            let chg0 = (ics.window_sequence == fd::EIGHT_SHORT)
                != (ics.window_sequence_prev == fd::EIGHT_SHORT);
            if chg0 {
                stereo.reset_prev_alphas();
            }

            stereo.ms_mask_mode = bs.read_bits_leq32(2)? as u8;
            stereo.ms_mask.fill(false);

            if stereo.ms_mask_mode == 1 {
                let max_sfb_ste = ics.max_sfb.max(ics2.max_sfb);
                for g in 0..ics.num_window_groups {
                    for sfb in 0..max_sfb_ste {
                        stereo.ms_mask[g * max_sfb_ste + sfb] = bs.read_bool()?;
                    }
                }
            }
            else if stereo.ms_mask_mode == 2 {
                stereo.ms_mask.fill(true);
            }
            else if stereo.ms_mask_mode == 3 && state.stereo_config_index == 0 {
                stereo.max_sfb_ste = ics.max_sfb.max(ics2.max_sfb);
                stereo.decode_complex_pred(bs, &ics, indep_flag)?;
            }

            // TNS.
            if tns_active {
                let common_tns = bs.read_bool()?;
                tns_on_lr = bs.read_bool()?;

                if common_tns {
                    // Decode once and share between the two channels.
                    let mut tns = tns::Tns::new();
                    tns.set_present();
                    tns.read_data(bs, ics.num_windows, ics.window_sequence == fd::EIGHT_SHORT)?;
                    shared_tns = Some(tns);
                }
                else if bs.read_bool()? {
                    tns_data_present0 = true;
                    tns_data_present1 = true;
                }
                else {
                    tns_data_present1 = bs.read_bool()?;
                    tns_data_present0 = !tns_data_present1;
                }
            }
            else {
                state.ch0.tns.reset();
                state.ch1.tns.reset();
            }

            state.ch0.parse(bs, indep_flag, noise_fill, tns_data_present0, Some(&ics))?;
            state.ch1.parse(bs, indep_flag, noise_fill, tns_data_present1, Some(&ics2))?;

            // `parse()` clears the TNS data of a channel that has none of its own.
            if let Some(tns) = shared_tns {
                state.ch0.tns = tns;
                state.ch1.tns = tns;
            }
        }
        else {
            // Independent ics_info() per channel.
            if tns_active {
                tns_on_lr = bs.read_bool()?;
                if bs.read_bool()? {
                    tns_data_present0 = true;
                    tns_data_present1 = true;
                }
                else {
                    tns_data_present1 = bs.read_bool()?;
                    tns_data_present0 = !tns_data_present1;
                }
            }

            state.ch0.parse(bs, indep_flag, noise_fill, tns_data_present0, None)?;
            state.ch1.parse(bs, indep_flag, noise_fill, tns_data_present1, None)?;
        }

        if let Some(sbr) = &mut state.sbr {
            sbr.parse(bs, data, indep_flag)?;
        }

        // The dequantized spectra of the two channels.
        let ics0 = state.ch0.ics.clone();
        let ics1 = state.ch1.ics.clone();
        let stride = ics0.window_len(ccfl);
        let max_sfb_ste = ics0.max_sfb.max(ics1.max_sfb);
        stereo.max_sfb_ste = max_sfb_ste;

        // Apply TNS before the stereo tools unless `tns_on_lr` (ISO/IEC 23003-3 7.7).
        if !tns_on_lr {
            state.ch0.tns.synth(
                ics0.swb_offset,
                ics0.max_sfb,
                ics0.tns_max_bands,
                ics0.num_windows,
                stride,
                &mut state.ch0.coeffs,
            );
            state.ch1.tns.synth(
                ics1.swb_offset,
                ics1.max_sfb,
                ics1.tns_max_bands,
                ics1.num_windows,
                stride,
                &mut state.ch1.coeffs,
            );
        }

        // Stereo tools.
        if stereo.ms_mask_mode == 3 && state.stereo_config_index == 0 {
            stereo.apply_complex_stereo(
                &ics0,
                stride,
                &mut state.ch0.coeffs,
                &mut state.ch1.coeffs,
                &state.ch0.prev_coeffs,
                &state.ch1.prev_coeffs,
            );
        }
        else if stereo.ms_mask_mode > 0 {
            stereo.apply_ms(&ics0, stride, &mut state.ch0.coeffs, &mut state.ch1.coeffs);
        }

        // Save the previous-frame alpha values for prediction (ISO/IEC 23003-3 7.7.2).
        state.stereo.store_prev_window_groups(ics0.num_window_groups);
        state.stereo.store_prev();

        // Apply TNS after the stereo tools when `tns_on_lr`.
        if tns_on_lr {
            state.ch0.tns.synth(
                ics0.swb_offset,
                ics0.max_sfb,
                ics0.tns_max_bands,
                ics0.num_windows,
                stride,
                &mut state.ch0.coeffs,
            );
            state.ch1.tns.synth(
                ics1.swb_offset,
                ics1.max_sfb,
                ics1.tns_max_bands,
                ics1.num_windows,
                stride,
                &mut state.ch1.coeffs,
            );
        }

        // The final spectra are the previous downmix of the next frame's complex prediction.
        state.ch0.prev_coeffs.copy_from_slice(&state.ch0.coeffs);
        state.ch1.prev_coeffs.copy_from_slice(&state.ch1.coeffs);

        self.filterbank.synth(&mut state.ch0, &mut state.pcm0);
        self.filterbank.synth(&mut state.ch1, &mut state.pcm1);

        let scale = 1.0 / PCM_SCALE;
        let [l, r] = state.targets;
        let Some((plane_l, plane_r)) = self.buf.plane_pair_mut(l, r)
        else {
            return decode_error("aac: invalid channel");
        };

        if let Some(sbr) = &mut state.sbr {
            return sbr.process(&[&state.pcm0, &state.pcm1], &mut [plane_l, plane_r], scale);
        }

        for (dst, src) in plane_l.iter_mut().zip(&state.pcm0) {
            *dst = src * scale;
        }
        for (dst, src) in plane_r.iter_mut().zip(&state.pcm1) {
            *dst = src * scale;
        }

        Ok(())
    }

    /// Read the payload of an extension element of this frame, if there is one.
    fn read_ext_element(
        ext: &UsacExtElementConfig,
        bs: &mut BitReaderLtr<'_>,
    ) -> Result<Option<Vec<u8>>> {
        if !bs.read_bool()? {
            // usacExtElementPresent
            return Ok(None);
        }

        let len = if bs.read_bool()? {
            // usacExtElementUseDefaultLength
            ext.default_length.unwrap_or(0)
        }
        else {
            let mut l = bs.read_bits_leq32(8)?;
            if l == 255 {
                l += bs.read_bits_leq32(16)? - 2;
            }
            l
        };

        if len == 0 {
            return Ok(None);
        }

        if ext.payload_frag {
            // usacExtElementStart / usacExtElementStop
            let _start = bs.read_bool()?;
            let _stop = bs.read_bool()?;
        }

        // Only the audio pre-roll payload is used; fill and DRC payloads are skipped.
        if ext.ext_type == UsacExtType::AudioPreroll {
            if bs.bits_left() < u64::from(len) * 8 {
                return decode_error("aac: usac extension payload is truncated");
            }

            let mut payload = Vec::with_capacity(len as usize);
            for _ in 0..len {
                payload.push(bs.read_bits_leq32(8)? as u8);
            }
            return Ok(Some(payload));
        }

        bs.ignore_bits(len * 8)?;

        Ok(None)
    }
}

impl AudioDecoder for UsacDecoder {
    fn reset(&mut self) {
        self.primed = false;
        for element in &mut self.elements {
            match element {
                Element::Sce(state) => {
                    state.ch.reset();
                    state.pcm.fill(0.0);
                    state.lpd.reset();
                    state.last_lpd = None;
                    if let Some(sbr) = &mut state.sbr {
                        sbr.reset();
                    }
                }
                Element::Cpe(state) => {
                    state.ch0.reset();
                    state.ch1.reset();
                    state.stereo.reset();
                    state.pcm0.fill(0.0);
                    state.pcm1.fill(0.0);
                    if let Some(sbr) = &mut state.sbr {
                        sbr.reset();
                    }
                }
            }
        }
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(
        &mut self,
        packet: &PacketRef<'_>,
    ) -> Result<symphonia_core::audio::GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_frame(packet) {
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

    fn last_decoded(&self) -> symphonia_core::audio::GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for UsacDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(UsacDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        use symphonia_core::codecs::audio::well_known::profiles::CODEC_PROFILE_AAC_USAC;

        &[support_audio_codec!(
            CODEC_ID_AAC,
            "aac",
            "Advanced Audio Coding",
            &[codec_profile!(
                CODEC_PROFILE_AAC_USAC,
                "aac-usac",
                "Unified Speech and Audio Coding"
            )]
        )]
    }
}
