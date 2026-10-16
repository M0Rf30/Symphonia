// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Range;

use symphonia_core::audio::{AsGenericAudioBufferRef, Audio, AudioBuffer, GenericAudioBufferRef};
use symphonia_core::audio::{AudioMut, AudioSpec};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, reset_error, unsupported_error};
use symphonia_core::io::FiniteStream;
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::common::{FrameHeader, Layer, MpegLayer};
use crate::header;
use crate::layer3::{IntensityMode, Layer3};

use super::{SbrDsp, SbrParser};

/// The number of core samples per MPEG-2/2.5 Layer III frame, and per SBR frame.
const CORE_FRAME_LEN: usize = 576;

/// The delay, in samples at the output rate, that the SBR filter banks add to the output with
/// respect to the core audio upsampled by two (481 core samples). Measured against the core
/// decoded by [`crate::MpaDecoder`] and equal to the delay of the Thomson mp3PRO decoder.
const SBR_DELAY: usize = 962;

/// The number of samples per channel the SBR path outputs for one frame.
const SBR_FRAME_LEN: usize = 2 * CORE_FRAME_LEN;

/// The SBR decoder state of a stream.
struct Sbr {
    parser: SbrParser,
    dsp: SbrDsp,
    /// SBR was switched off permanently. The core-only path is used from then on.
    disabled: bool,
}

/// mp3PRO decoder: MPEG Layer III with Spectral Band Replication.
///
/// The core is decoded as MPEG Layer III with the mp3PRO intensity stereo rule (see
/// `IntensityMode::Mp3Pro`). If the core is an MPEG-2/2.5 stream with a sample rate below
/// 32 kHz, the SBR data carried in the ancillary data of every frame reconstructs the high band,
/// and the decoder outputs audio at twice the core sample rate. The SBR filter banks delay the
/// output by 962 samples (at the output rate). If gapless playback is enabled, the delay is
/// compensated: the packet trims, which count core samples, are doubled and moved 962 samples
/// later, so the audio that remains is the audio between the stream delay and padding, aligned
/// with the core. (The last 962 samples of the stream cannot be decoded, so padding shorter than
/// 481 core samples shortens the output.) Otherwise, the decoder outputs the core audio unchanged.
///
/// For a plain MP3 stream, the output differs from the ISO decoder [`crate::MpaDecoder`] only in
/// the treatment of MPEG-2/2.5 intensity stereo, and, for streams below 32 kHz, the doubled
/// sample rate. This decoder is not registered by default.
pub struct Mp3ProDecoder {
    opts: AudioDecoderOptions,
    params: AudioCodecParameters,
    layer: Box<Layer3>,
    sbr: Option<Box<Sbr>>,
    /// The core audio of the current frame (SBR path only).
    core: AudioBuffer<f32>,
    buf: AudioBuffer<f32>,
    /// The signal specification of the core audio of the stream being decoded. The output
    /// specification alone does not identify the stream: an MPEG-2 core at 22.05 kHz with SBR and
    /// an MPEG-1 core at 44.1 kHz without it both output 44.1 kHz.
    core_spec: Option<AudioSpec>,
    /// The range of the first samples of the next SBR packet that the delayed audio of the
    /// previous packet leaves in the output, if gapless playback is enabled.
    carry: Range<usize>,
}

impl Mp3ProDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_MP3 {
            return unsupported_error("mp3pro: invalid codec");
        }

        Ok(Mp3ProDecoder {
            opts: *opts,
            params: params.clone(),
            layer: Box::new(Layer3::new(IntensityMode::Mp3Pro)),
            sbr: None,
            core: Default::default(),
            buf: Default::default(),
            core_spec: None,
            carry: 0..0,
        })
    }

    /// SBR is used for MPEG-2/2.5 cores with a sample rate below 32 kHz.
    fn has_sbr(header: &FrameHeader) -> bool {
        !header.is_mpeg1() && header.sample_rate < 32000
    }

    /// Trims the SBR output of a packet by the packet's trims, which count core samples. The
    /// audio of a packet is delayed by `SBR_DELAY` samples, so the first samples of the output
    /// belong to the previous packet, and the end of the audio of this packet is in the next one.
    fn trim_sbr(&mut self, trim_start: usize, trim_end: usize) {
        let (trim_start, trim_end) = (2 * trim_start, 2 * trim_end);

        // The kept samples of the previous packet's audio, then of this packet's audio.
        let prev = self.carry.clone();
        let own = trim_start.saturating_add(SBR_DELAY).min(SBR_FRAME_LEN)
            ..(SBR_FRAME_LEN + SBR_DELAY).saturating_sub(trim_end).clamp(SBR_DELAY, SBR_FRAME_LEN);

        // The samples of this packet's audio that the next packet holds.
        self.carry = (trim_start + SBR_DELAY).saturating_sub(SBR_FRAME_LEN).min(SBR_DELAY)
            ..SBR_DELAY.saturating_sub(trim_end);

        // The kept samples are contiguous: trims only remove the start and the end of the stream.
        let (start, end) = match (prev.is_empty(), own.is_empty()) {
            (false, false) => (prev.start, own.end),
            (false, true) => (prev.start, prev.end),
            (true, false) => (own.start, own.end),
            (true, true) => (SBR_FRAME_LEN, SBR_FRAME_LEN),
        };

        self.buf.trim(start, SBR_FRAME_LEN - end);
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let mut reader = packet.as_buf_reader();

        let header =
            header::parse_frame_header_packet(header::sync_frame(&mut reader)?, packet.data.len())?;

        if header.frame_size != reader.bytes_available() as usize {
            return decode_error("mp3pro: invalid packet length");
        }

        if header.layer != MpegLayer::Layer3 {
            return decode_error("mp3pro: invalid mpeg audio layer");
        }

        let core_spec = header.spec();
        let sbr = Self::has_sbr(&header);

        let out_spec = if sbr {
            AudioSpec::new(2 * core_spec.rate(), core_spec.channels().clone())
        }
        else {
            core_spec.clone()
        };

        // The audio buffer can only be created after the first frame is decoded.
        if self.buf.is_unused() {
            self.buf = AudioBuffer::new(out_spec, 1152);
            self.core_spec = Some(core_spec.clone());
        }
        else if self.core_spec.as_ref() != Some(&core_spec) {
            // The packet contains an audio frame with a different signal specification than the
            // stream so far (e.g., concatenated streams). Discard the state of the previous
            // stream and report that the audio specification is about to change. The decoder is
            // ready to decode this packet, or the next one, with the new specification.
            self.reset();
            self.core = Default::default();
            self.buf = Default::default();
            self.core_spec = None;
            self.params
                .with_sample_rate(core_spec.rate())
                .with_channels(core_spec.channels().clone());

            return reset_error();
        }

        self.buf.clear();

        self.layer.verify_crc(packet.data, &header)?;

        if !sbr {
            self.layer.decode(&mut reader, &header, &mut self.buf)?;
        }
        else {
            if self.core.is_unused() {
                self.core = AudioBuffer::new(core_spec, CORE_FRAME_LEN);
            }

            self.core.clear();
            self.layer.decode(&mut reader, &header, &mut self.core)?;

            let n_channels = header.n_channels();
            let out_rate = 2 * header.sample_rate;

            let sbr = self.sbr.get_or_insert_with(|| {
                let parser = SbrParser::new(header.sample_rate, n_channels);
                let dsp = SbrDsp::new(n_channels, out_rate, parser.header(), parser.tables());
                Box::new(Sbr { parser, dsp, disabled: false })
            });

            let mut apply = false;

            if !sbr.disabled {
                let status = sbr.parser.parse(self.layer.ancillary_data());

                if status.header_reset {
                    let data_extra = sbr.parser.frame(0).data_extra;
                    sbr.dsp.reset(sbr.parser.header(), sbr.parser.tables(), data_extra);
                }

                sbr.disabled = status.disabled;
                apply = status.apply && !status.disabled;
            }

            self.buf.render_uninit(Some(2 * CORE_FRAME_LEN));

            for ch in 0..n_channels {
                let (Some(core), Some(out)) = (self.core.plane(ch), self.buf.plane_mut(ch))
                else {
                    return decode_error("mp3pro: missing audio plane");
                };

                let frame = if apply { Some(sbr.parser.frame(ch)) } else { None };

                sbr.dsp.process(ch, core, frame, sbr.parser.header(), out);
            }
        }

        if self.opts.gapless {
            // The trim counts are in core samples.
            let trim_start = packet.trim_start.get() as usize;
            let trim_end = packet.trim_end.get() as usize;

            if sbr {
                self.trim_sbr(trim_start, trim_end);
            }
            else {
                self.buf.trim(trim_start, trim_end);
            }
        }

        Ok(())
    }
}

impl AudioDecoder for Mp3ProDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn reset(&mut self) {
        *self.layer = Layer3::new(IntensityMode::Mp3Pro);
        self.sbr = None;
        self.carry = 0..0;
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

impl RegisterableAudioDecoder for Mp3ProDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Mp3ProDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_MP3, "mp3pro", "mp3PRO (MPEG Audio Layer 3 with SBR)")]
    }
}
