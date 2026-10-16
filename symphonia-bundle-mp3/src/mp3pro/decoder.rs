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
use symphonia_core::io::{BufReader, FiniteStream};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::common::{FrameHeader, Layer, MpegLayer};
use crate::header;
use crate::layer3::{IntensityMode, Layer3};

use super::{ParseStatus, SbrDsp, SbrParser};

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
    /// The filter banks, created once the output of the stream is known to be the SBR output.
    dsp: Option<SbrDsp>,
}

/// mp3PRO decoder: MPEG Layer III with Spectral Band Replication.
///
/// A stream is an mp3PRO stream if it is an MPEG-2/2.5 Layer III stream with a sample rate below
/// 32 kHz, and the ancillary data of its frames carries SBR data (a valid SBR header). The SBR
/// data reconstructs the high band, and the decoder outputs audio at twice the core sample rate.
/// The core is decoded with the mp3PRO intensity stereo rule (see `IntensityMode::Mp3Pro`). The
/// SBR filter banks delay the output by 962 samples (at the output rate). If gapless playback is
/// enabled, the delay is compensated: the packet trims, which count core samples, are doubled and
/// moved 962 samples later, so the audio that remains is the audio between the stream delay and
/// padding, aligned with the core. (The last 962 samples of the stream cannot be decoded, so
/// padding shorter than 481 core samples shortens the output.) Otherwise, the decoder outputs the
/// core audio unchanged.
///
/// Every other stream is a plain MP3 stream: the decoder outputs the same samples as the ISO
/// decoder [`crate::MpaDecoder`], at the core sample rate. Whether a stream is an mp3PRO stream is
/// decided by its first frame. If the first frame has no SBR data but a later frame has, the
/// decoder requests a reset ([`symphonia_core::errors::Error::ResetRequired`]) as the output
/// sample rate changes.
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
    /// Whether the output of the stream is the SBR output at twice the core sample rate. `None`
    /// until the first frame of the stream that may carry SBR data is decoded.
    sbr_output: Option<bool>,
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
            layer: Box::new(Layer3::new()),
            sbr: None,
            core: Default::default(),
            buf: Default::default(),
            core_spec: None,
            sbr_output: None,
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

        if self.core_spec.is_none() {
            self.core_spec = Some(core_spec.clone());
        }
        else if self.core_spec.as_ref() != Some(&core_spec) {
            // The packet contains an audio frame with a different signal specification than the
            // stream so far (e.g., concatenated streams). Discard the state of the previous
            // stream and report that the audio specification is about to change. The decoder is
            // ready to decode this packet, or the next one, with the new specification.
            self.reset();
            self.discard_stream();
            self.sbr_output = None;
            self.params
                .with_sample_rate(core_spec.rate())
                .with_channels(core_spec.channels().clone());

            return reset_error();
        }

        self.layer.verify_crc(packet.data, &header)?;

        let sbr_output = if Self::has_sbr(&header) {
            self.decode_sbr_capable(&mut reader, &header, core_spec)?
        }
        else {
            self.sbr_output = Some(false);

            if self.buf.is_unused() {
                self.buf = AudioBuffer::new(core_spec, 1152);
            }

            self.buf.clear();
            self.layer.decode(&mut reader, &header, &mut self.buf)?;
            false
        };

        if self.opts.gapless {
            // The trim counts are in core samples.
            let trim_start = packet.trim_start.get() as usize;
            let trim_end = packet.trim_end.get() as usize;

            if sbr_output {
                self.trim_sbr(trim_start, trim_end);
            }
            else {
                self.buf.trim(trim_start, trim_end);
            }
        }

        Ok(())
    }

    /// Discards the audio buffers and the signal specification of the stream being decoded.
    fn discard_stream(&mut self) {
        self.core = Default::default();
        self.buf = Default::default();
        self.core_spec = None;
    }

    /// Decodes a frame of a core that may carry SBR data, into `self.buf`. Returns true if the
    /// output is the SBR output at twice the core rate, and false if it is the core audio.
    ///
    /// Whether the stream has SBR data is not known before the ancillary data of its frames is
    /// parsed, which requires decoding the frame (the ancillary data follows the main data, which
    /// is in the bit reservoir). The first frame of the stream with valid SBR data, which carries
    /// an SBR header, makes the stream an mp3PRO stream. Until then, the frames are decoded like
    /// plain MPEG-2/2.5 Layer III, like [`crate::MpaDecoder`] does: the intensity stereo rule of
    /// a frame is chosen after the ancillary data is parsed and before the stereo processing. If
    /// the first frame of the stream has SBR data, the stream is an mp3PRO stream from the start.
    /// Otherwise, the stream is a plain stream, and if SBR data is found later (a stream that
    /// starts without an SBR header), the change of the output specification is reported with a
    /// reset, and the frame is decoded again with the new specification.
    fn decode_sbr_capable(
        &mut self,
        reader: &mut BufReader<'_>,
        header: &FrameHeader,
        core_spec: AudioSpec,
    ) -> Result<bool> {
        if self.core.is_unused() {
            self.core = AudioBuffer::new(core_spec.clone(), 1152);
        }

        self.core.clear();

        let n_channels = header.n_channels();
        let known = self.sbr_output;

        let sbr = self.sbr.get_or_insert_with(|| {
            Box::new(Sbr { parser: SbrParser::new(header.sample_rate, n_channels), dsp: None })
        });

        // Parse the ancillary data of the frame as soon as it is available.
        let mut status = ParseStatus::default();

        self.layer.decode_frame(reader, header, &mut self.core, |ancillary| {
            status = sbr.parser.parse(ancillary);

            // The frame is decoded as an mp3PRO frame if the stream is one, or if this frame
            // makes it one.
            if known.unwrap_or(status.apply && status.clean) {
                IntensityMode::Mp3Pro
            }
            else {
                IntensityMode::Iso
            }
        })?;

        let apply = status.apply && !status.disabled;

        // Only a block that parses without errors starts the SBR output of a stream.
        let detected = apply && status.clean;
        let sbr_output = *self.sbr_output.get_or_insert(detected);

        if !sbr_output {
            if detected {
                // SBR data in a stream that was decoded as a plain stream so far.
                self.reset();
                self.discard_stream();
                self.sbr_output = Some(true);
                return reset_error();
            }

            // The parser switches SBR off for good if the data is not valid SBR data. That is
            // what a plain stream looks like, but it must not stop the detection of SBR data in
            // the frames to come.
            if status.disabled {
                self.sbr = None;
            }

            // The core audio is the output.
            std::mem::swap(&mut self.core, &mut self.buf);
            return Ok(false);
        }

        if self.buf.is_unused() {
            let out_spec = AudioSpec::new(2 * core_spec.rate(), core_spec.channels().clone());
            self.buf = AudioBuffer::new(out_spec, 1152);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(SBR_FRAME_LEN));

        let out_rate = 2 * header.sample_rate;

        let dsp = sbr.dsp.get_or_insert_with(|| {
            SbrDsp::new(n_channels, out_rate, sbr.parser.header(), sbr.parser.tables())
        });

        if status.header_reset {
            let data_extra = sbr.parser.frame(0).data_extra;
            dsp.reset(sbr.parser.header(), sbr.parser.tables(), data_extra);
        }

        for ch in 0..n_channels {
            let (Some(core), Some(out)) = (self.core.plane(ch), self.buf.plane_mut(ch))
            else {
                return decode_error("mp3pro: missing audio plane");
            };

            let frame = if apply { Some(sbr.parser.frame(ch)) } else { None };

            dsp.process(ch, core, frame, sbr.parser.header(), out);
        }

        Ok(true)
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
        *self.layer = Layer3::new();
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
