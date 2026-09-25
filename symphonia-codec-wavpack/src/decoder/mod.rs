// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod bits;
mod words;
mod v3;
mod v4v5;
mod floats;

use symphonia_core::audio::{AsGenericAudioBufferRef, AudioSpec, GenericAudioBuffer, GenericAudioBufferRef};
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult};
use symphonia_core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use v3::{DecorrPass, DcState, unpack_init3, unpack_samples_v3, MONO_FLAG};
use words::WordState;
use v4v5::{
    DecorrPass as DecorrPass45, WordsState as WordsState45,
    PACKET_MAGIC, STREAM_HDR,
    parse_decorr_terms, parse_decorr_weights, parse_decorr_samples,
    parse_entropy_vars, parse_int32_info,
    unpack_samples_v4v5,
};

/// One physical wvpk block's decoded samples: a stream is a mono or stereo pair of
/// output channels; a multichannel "block group" is made of several of these (see
/// `WavPackDecoder::decode_inner_v4v5`).
struct StreamDecode {
    /// Interleaved decoded samples: `block_samples` long if mono, `block_samples * 2`
    /// if stereo (or false-stereo).
    samples: Vec<i32>,
    /// Number of *output* channels this stream contributes (1 or 2). Note this is 2 for
    /// false-stereo blocks too, even though only one data channel was decoded.
    channel_count: usize,
    /// `FALSE_STEREO` (data-wise mono, but occupies two output channel slots): both
    /// output channels should read the single decoded data channel.
    is_false_stereo: bool,
    /// Per-channel frame count for this stream's block, as declared by its header.
    block_samples: u32,
}

// Packet-prefix layout (32 bytes) — written by the format reader:
//   [0..2]   version      i16 LE
//   [2..4]   bits         i16 LE  (0 = lossless)
//   [4..6]   flags        i16 LE
//   [6..8]   shift        i16 LE
//   [8..12]  total_samples i32 LE
//   [12..16] crc          i32 LE
//   [16..20] crc2         i32 LE
//   [20..24] ext[4]
//   [24]     extra_bc
//   [25..28] extras[3]
//   [28..30] num_channels u16 LE
//   [30..32] bytes_per_sample u16 LE
//   [32..]   compressed audio
const PREFIX_LEN: usize = 32;

#[derive(Debug)]
struct BlockHeader {
    version:       i16,
    bits:          i16,
    flags:         i16,
    shift:         i16,
    total_samples: i32,
    crc:           i32,
    #[allow(dead_code)]
    crc2:          i32,
    num_channels:  u16,
    #[allow(dead_code)]
    bytes_per_sample: u16,
}

fn parse_prefix(data: &[u8]) -> Option<(BlockHeader, &[u8])> {
    if data.len() < PREFIX_LEN {
        return None;
    }
    let hdr = BlockHeader {
        version:          i16::from_le_bytes([data[0],  data[1]]),
        bits:             i16::from_le_bytes([data[2],  data[3]]),
        flags:            i16::from_le_bytes([data[4],  data[5]]),
        shift:            i16::from_le_bytes([data[6],  data[7]]),
        total_samples:    i32::from_le_bytes([data[8],  data[9],  data[10], data[11]]),
        crc:              i32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        crc2:             i32::from_le_bytes([data[16], data[17], data[18], data[19]]),
        // [20..24] ext, [24] extra_bc, [25..28] extras — unused in decode
        num_channels:     u16::from_le_bytes([data[28], data[29]]),
        bytes_per_sample: u16::from_le_bytes([data[30], data[31]]),
    };
    Some((hdr, &data[PREFIX_LEN..]))
}

// ---------------------------------------------------------------------------
// WavPackDecoder
// ---------------------------------------------------------------------------

pub struct WavPackDecoder {
    params:      AudioCodecParameters,
    // v3 per-stream state
    dc:          DcState,
    decorr:      Vec<DecorrPass>,
    num_terms:   usize,
    word_state:  WordState,
    initialized: bool,
    last_flags:  i16,
    // Output buffer
    buf: GenericAudioBuffer,
}

impl WavPackDecoder {
    pub fn try_new(params: &AudioCodecParameters, _opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_WAVPACK {
            return unsupported_error("wavpack decoder: wrong codec id");
        }

        let rate = params.sample_rate.unwrap_or(44100);
        let channels = match &params.channels {
            Some(ch) => ch.clone(),
            None => return unsupported_error("wavpack decoder: no channel info"),
        };
        let sample_format = params.sample_format.unwrap_or(SampleFormat::S32);
        let spec = AudioSpec::new(rate, channels);
        let buf = GenericAudioBuffer::new(sample_format, spec, 0);

        Ok(WavPackDecoder {
            params: params.clone(),
            dc: DcState::default(),
            decorr: Vec::new(),
            num_terms: 0,
            word_state: WordState::default(),
            initialized: false,
            last_flags: 0,
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // Dispatch on packet type: v4/v5 packets start with "WV45" magic.
        if packet.data.starts_with(PACKET_MAGIC) {
            return self.decode_inner_v4v5(packet);
        }

        let data = packet.data;
        let (hdr, audio) = parse_prefix(data)
            .ok_or(symphonia_core::errors::Error::DecodeError("wavpack: packet too short"))?;

        if hdr.total_samples <= 0 {
            self.buf.clear();
            return Ok(());
        }
        let sample_count = hdr.total_samples as u32;
        let num_channels = hdr.num_channels as u32;

        // First block (or after reset): initialise decorr passes
        if !self.initialized || hdr.flags != self.last_flags {
            unpack_init3(hdr.flags, &mut self.decorr, &mut self.num_terms);
            self.dc = DcState::default();
            self.word_state = WordState::default();
            self.initialized = true;
            self.last_flags = hdr.flags;
        }

        // Decode
        let samples = unpack_samples_v3(
            hdr.version,
            hdr.bits,
            hdr.flags,
            hdr.shift,
            sample_count,
            num_channels,
            audio,
            &mut self.dc,
            &mut self.decorr,
            self.num_terms,
            &mut self.word_state,
        );

        if samples.is_empty() && sample_count > 0 {
            return decode_error("wavpack: no samples decoded (unsupported flags?)");
        }

        let is_mono = (hdr.flags & MONO_FLAG) != 0;
        let decoded_frames = if is_mono { samples.len() } else { samples.len() / 2 };

        // Grow / reset buffer
        self.buf.clear();
        match &mut self.buf {
            GenericAudioBuffer::S16(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    if is_mono {
                        planes[0][idx] = samples[idx].clamp(-32768, 32767) as i16;
                    } else {
                        planes[0][idx] = samples[idx * 2    ].clamp(-32768, 32767) as i16;
                        planes[1][idx] = samples[idx * 2 + 1].clamp(-32768, 32767) as i16;
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S32(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    if is_mono {
                        planes[0][idx] = samples[idx];
                    } else {
                        planes[0][idx] = samples[idx * 2];
                        planes[1][idx] = samples[idx * 2 + 1];
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S24(b) => {
                use symphonia_core::audio::sample::i24;
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    let clamp = |v: i32| i24::from(v.clamp(-8_388_608, 8_388_607));
                    if is_mono {
                        planes[0][idx] = clamp(samples[idx]);
                    } else {
                        planes[0][idx] = clamp(samples[idx * 2]);
                        planes[1][idx] = clamp(samples[idx * 2 + 1]);
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S8(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    if is_mono {
                        planes[0][idx] = samples[idx].clamp(-128, 127) as i8;
                    } else {
                        planes[0][idx] = samples[idx * 2    ].clamp(-128, 127) as i8;
                        planes[1][idx] = samples[idx * 2 + 1].clamp(-128, 127) as i8;
                    }
                    Ok(())
                })?;
            }
            _ => return unsupported_error("wavpack decoder: unsupported output sample format"),
        }

        // Optional CRC check (version 3 only, lossless)
        if hdr.version == 3 && hdr.bits == 0 && self.dc.crc != hdr.crc {
            log::warn!("wavpack: CRC mismatch (expected {:08x}, got {:08x})", hdr.crc, self.dc.crc);
        }

        Ok(())
    }

    /// Decode one physical wvpk block (one stream within a possibly-multichannel "block
    /// group") from its bounded, self-contained slice (see `StreamBlock` layout in
    /// reader/mod.rs's `read_v4v5_stream_block`). Per-block decorrelation/entropy state
    /// (`decorr`/`words`) is local: v4/v5 blocks always resend their coefficients and
    /// medians from scratch, so there is nothing to persist across calls.
    fn decode_v4v5_stream(stream_data: &[u8]) -> Result<StreamDecode> {
        if stream_data.len() < STREAM_HDR {
            return decode_error("wavpack v4/v5: stream block too short");
        }

        let flags         = u32::from_le_bytes([stream_data[0], stream_data[1], stream_data[2], stream_data[3]]);
        let block_samples = u32::from_le_bytes([stream_data[4], stream_data[5], stream_data[6], stream_data[7]]);
        let read_u32 = |off: usize| {
            u32::from_le_bytes([stream_data[off], stream_data[off+1], stream_data[off+2], stream_data[off+3]]) as usize
        };
        let tl  = read_u32(12);
        let wl  = read_u32(16);
        let sl  = read_u32(20);
        let el  = read_u32(24);
        let hpl = read_u32(28); // hybrid profile
        let fil = read_u32(32); // float info
        let il  = read_u32(36); // int32 info
        let wxl = read_u32(40); // wvx (float/int32 extension) bitstream

        let mut pos = STREAM_HDR;
        let end     = stream_data.len();

        let terms_raw   = &stream_data[pos..pos.saturating_add(tl).min(end)];  pos += tl;
        let weights_raw = &stream_data[pos..pos.saturating_add(wl).min(end)];  pos += wl;
        let samples_raw = &stream_data[pos..pos.saturating_add(sl).min(end)];  pos += sl;
        let entropy_raw = &stream_data[pos..pos.saturating_add(el).min(end)];  pos += el;
        let hybrid_raw  = &stream_data[pos..pos.saturating_add(hpl).min(end)]; pos += hpl;
        let float_raw   = &stream_data[pos..pos.saturating_add(fil).min(end)]; pos += fil;
        let int32_raw   = &stream_data[pos..pos.saturating_add(il).min(end)];  pos += il;
        let wvx_raw     = &stream_data[pos..pos.saturating_add(wxl).min(end)]; pos += wxl;
        let audio       = &stream_data[pos.min(end)..];

        let is_mono = (flags & v4v5::MONO_DATA) != 0;
        // WavPack's "false stereo" optimization: both channels are identical, so the encoder
        // stores a single decoded channel (FALSE_STEREO set, MONO_FLAG clear) but the track is
        // still reported with two output channels; duplicate the decoded channel into both
        // planes so silence isn't produced on the second channel.
        let is_true_mono = (flags & v4v5::MONO_FLAG) != 0;
        let is_false_stereo = is_mono && !is_true_mono;
        let is_float = (flags & v4v5::FLOAT_DATA) != 0;

        let mut decorr: Vec<DecorrPass45> = Vec::new();
        let mut words = WordsState45::default();

        parse_decorr_terms(terms_raw, &mut decorr);
        parse_decorr_weights(weights_raw, &mut decorr, is_mono);
        parse_decorr_samples(samples_raw, &mut decorr, is_mono);
        parse_entropy_vars(entropy_raw, &mut words, is_mono);
        if (flags & v4v5::HYBRID_FLAG) != 0 {
            v4v5::parse_hybrid_profile(hybrid_raw, &mut words, is_mono, flags);
        }
        let i32info = parse_int32_info(int32_raw);
        let float_info = if is_float { floats::parse_float_info(float_raw) } else { None };

        let samples = if block_samples == 0 {
            Vec::new()
        }
        else {
            unpack_samples_v4v5(
                flags, block_samples,
                &mut decorr,
                &mut words,
                &i32info,
                float_info.as_ref(),
                wvx_raw,
                audio,
            ).ok_or(symphonia_core::errors::Error::DecodeError(
                "wavpack v4/v5: unsupported encoding"
            ))?
        };

        if samples.is_empty() && block_samples > 0 {
            return decode_error("wavpack v4/v5: no samples decoded");
        }

        Ok(StreamDecode {
            samples,
            channel_count: if is_true_mono { 1 } else { 2 },
            is_false_stereo,
            block_samples,
        })
    }

    /// Decode a packet built by `WavPackReader::next_packet_v4v5`: one or more per-stream
    /// blocks (a "block group") sharing the same starting sample index, one block per
    /// mono/stereo audio stream. For an ordinary mono/stereo file this is a single
    /// stream; multichannel files (>2 channels) interleave several streams' worth of
    /// samples into the final output exactly like WavPack's own
    /// `unpack_samples_interleave()` does.
    fn decode_inner_v4v5(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;
        if data.len() < 8 {
            return decode_error("wavpack v4/v5: packet too short");
        }
        let stream_count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let mut pos = 8usize;
        let end = data.len();

        let mut streams: Vec<StreamDecode> = Vec::with_capacity(stream_count.min(64));
        for _ in 0..stream_count {
            if pos + 4 > end {
                break; // truncated packet: decode whatever complete streams remain
            }
            let slen = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;
            let stream_end = pos.saturating_add(slen).min(end);
            let stream_data = &data[pos..stream_end];
            pos = stream_end;

            streams.push(Self::decode_v4v5_stream(stream_data)?);
        }

        if streams.is_empty() || streams[0].block_samples == 0 {
            self.buf.clear();
            return Ok(());
        }

        let decoded_frames = streams[0].block_samples as usize;

        // Map each OUTPUT channel (in file order) to (stream index, this stream's local
        // data-channel 0/1), so any channel count (mono/stereo/quad/5.1/7.1/...)
        // interleaves the same way WavPack's own `unpack_samples_interleave()` does.
        let mut plane_source: Vec<(usize, usize)> = Vec::with_capacity(streams.len() * 2);
        for (i, s) in streams.iter().enumerate() {
            plane_source.push((i, 0));
            if s.channel_count == 2 {
                plane_source.push((i, if s.is_false_stereo { 0 } else { 1 }));
            }
        }

        // Defensive against malformed/truncated multichannel input (a stream that
        // decoded fewer samples than its declared `block_samples`): read back zero
        // rather than panicking on an out-of-bounds index.
        let sample_at = |stream_idx: usize, local_ch: usize, frame: usize| -> i32 {
            let s = &streams[stream_idx];
            let idx = if s.channel_count == 1 { frame } else { frame * 2 + local_ch };
            s.samples.get(idx).copied().unwrap_or(0)
        };

        self.buf.clear();
        match &mut self.buf {
            GenericAudioBuffer::S16(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = sample_at(si, ch, idx).clamp(-32768, 32767) as i16;
                        }
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S32(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = sample_at(si, ch, idx);
                        }
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S24(b) => {
                use symphonia_core::audio::sample::i24;
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = i24::from(sample_at(si, ch, idx).clamp(-8_388_608, 8_388_607));
                        }
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::S8(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = sample_at(si, ch, idx).clamp(-128, 127) as i8;
                        }
                    }
                    Ok(())
                })?;
            }
            GenericAudioBuffer::F32(b) => {
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = f32::from_bits(sample_at(si, ch, idx) as u32);
                        }
                    }
                    Ok(())
                })?;
            }
            _ => return unsupported_error("wavpack v4/v5: unsupported output sample format"),
        }

        Ok(())
    }
}

impl AudioDecoder for WavPackDecoder {
    fn reset(&mut self) {
        self.initialized = false;
        self.dc = DcState::default();
        self.decorr.clear();
        self.num_terms = 0;
        self.word_state = WordState::default();
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()
            .iter()
            .find(|d| d.id == self.params.codec)
            .expect("at least one codec registered")
            .info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            return Err(e);
        }
        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for WavPackDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(WavPackDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_WAVPACK, "wavpack", "WavPack Lossless Audio (v1–v3)")]
    }
}
