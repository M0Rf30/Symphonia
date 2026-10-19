// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod bits;
mod dsd;
pub(crate) mod md5;
pub(crate) use dsd::dsd_rate_shift;
use md5::Md5Source;
mod floats;
mod v3;
mod v4v5;
mod words;

/// The largest block, in samples per channel, accepted by any of the unpackers. WavPack's own
/// encoder caps block size at 131072 samples (`--blocksize`); this is a generous multiple of
/// that, used to reject a malformed/mutated sample count before it can cause an arithmetic
/// overflow or an unbounded allocation.
pub(crate) const MAX_BLOCK_SAMPLES: u32 = 1 << 20;

/// The number of samples of history kept per decorrelation pass (the largest "short" term).
pub(crate) const MAX_TERM: usize = 8;

/// Fold `value` into a running WavPack checksum.
#[inline(always)]
fn crc_accumulate(crc: u32, value: u32) -> u32 {
    crc.wrapping_mul(3).wrapping_add(value)
}

use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioSpec, GenericAudioBuffer, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult, VerificationCheck,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::block::{
    BlockHeader, DSD_FLAG, FALSE_STEREO, FLOAT_DATA, HEADER_LEN, HYBRID_FLAG, ID_DECORR_SAMPLES,
    ID_DECORR_TERMS, ID_DECORR_WEIGHTS, ID_DSD_BLOCK, ID_ENTROPY_VARS, ID_FLOAT_INFO,
    ID_HYBRID_PROFILE, ID_INT32_INFO, ID_SHAPING_WEIGHTS, ID_WV_BITSTREAM, ID_WVC_BITSTREAM,
    ID_WVX_BITSTREAM, ID_WVX_NEW_BITSTREAM, MONO_DATA, MONO_FLAG as MONO_FLAG45, SubBlocks,
    verify_block,
};
use crate::reader::matroska_block_to_packet;

use v3::{DcState, MONO_FLAG, unpack_init3, unpack_samples_v3};
use v4v5::{
    DecorrPass, UnpackError, UnpackScratch, WordsState as WordsState45, WvcInput, WvxInput,
    parse_decorr_samples, parse_decorr_terms, parse_decorr_weights, parse_entropy_vars,
    parse_int32_info, unpack_samples_v4v5,
};
use words::WordState;

/// The magic number at the start of a v4/v5 packet (see `WavPackReader::next_packet_v4v5`), which
/// tells it from a v3 packet.
pub(crate) const PACKET_MAGIC: &[u8; 4] = b"WV45";

/// One physical wvpk block's decoded samples: a stream is a mono or stereo pair of
/// output channels; a multichannel "block group" is made of several of these (see
/// `WavPackDecoder::decode_inner_v4v5`). The sample buffer is reused from packet to packet.
#[derive(Default)]
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
    /// The block header flags.
    flags: u32,
    /// The samples are the exact original ones: the block is not a hybrid block decoded without
    /// its correction data.
    lossless: bool,
}

/// The sub-blocks of a main block or of a correction block that the decoder uses.
#[derive(Default)]
struct BlockParts<'a> {
    terms: &'a [u8],
    weights: &'a [u8],
    samples: &'a [u8],
    entropy: &'a [u8],
    hybrid: &'a [u8],
    float: &'a [u8],
    int32: &'a [u8],
    /// `ID_WVX_BITSTREAM` or `ID_WVX_NEW_BITSTREAM`, and whether it is the new kind.
    wvx: &'a [u8],
    wvx_new: bool,
    shaping: &'a [u8],
    /// `ID_WVC_BITSTREAM`: only in blocks of a `.wvc` file.
    wvc: &'a [u8],
    dsd: Option<&'a [u8]>,
    audio: &'a [u8],
}

impl<'a> BlockParts<'a> {
    fn new(body: &'a [u8]) -> Self {
        let mut parts = BlockParts::default();

        for sub_block in SubBlocks::new(body) {
            let data = sub_block.data;
            match sub_block.id {
                ID_DECORR_TERMS => parts.terms = data,
                ID_DECORR_WEIGHTS => parts.weights = data,
                ID_DECORR_SAMPLES => parts.samples = data,
                ID_ENTROPY_VARS => parts.entropy = data,
                ID_HYBRID_PROFILE => parts.hybrid = data,
                ID_FLOAT_INFO => parts.float = data,
                ID_INT32_INFO => parts.int32 = data,
                ID_SHAPING_WEIGHTS => parts.shaping = data,
                ID_WV_BITSTREAM => parts.audio = data,
                ID_WVC_BITSTREAM => parts.wvc = data,
                ID_WVX_BITSTREAM => {
                    parts.wvx = data;
                    parts.wvx_new = false;
                }
                ID_WVX_NEW_BITSTREAM => {
                    parts.wvx = data;
                    parts.wvx_new = true;
                }
                ID_DSD_BLOCK => parts.dsd = Some(data),
                _ => (),
            }
        }

        parts
    }
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
struct V3Header {
    version: i16,
    bits: i16,
    flags: i16,
    shift: i16,
    total_samples: i32,
    crc: i32,
    #[allow(dead_code)]
    crc2: i32,
    num_channels: u16,
    #[allow(dead_code)]
    bytes_per_sample: u16,
}

fn parse_prefix(data: &[u8]) -> Option<(V3Header, &[u8])> {
    if data.len() < PREFIX_LEN {
        return None;
    }
    let hdr = V3Header {
        version: i16::from_le_bytes([data[0], data[1]]),
        bits: i16::from_le_bytes([data[2], data[3]]),
        flags: i16::from_le_bytes([data[4], data[5]]),
        shift: i16::from_le_bytes([data[6], data[7]]),
        total_samples: i32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        crc: i32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        crc2: i32::from_le_bytes([data[16], data[17], data[18], data[19]]),
        // [20..24] ext, [24] extra_bc, [25..28] extras — unused in decode
        num_channels: u16::from_le_bytes([data[28], data[29]]),
        bytes_per_sample: u16::from_le_bytes([data[30], data[31]]),
    };
    Some((hdr, &data[PREFIX_LEN..]))
}

// ---------------------------------------------------------------------------
// WavPackDecoder
// ---------------------------------------------------------------------------

pub struct WavPackDecoder {
    params: AudioCodecParameters,
    // v3 per-stream state
    dc: DcState,
    decorr: Vec<DecorrPass>,
    num_terms: usize,
    word_state: WordState,
    // Decoded v3 samples, reused from packet to packet.
    v3_samples: Vec<i32>,
    // v4/v5 buffers reused from packet to packet.
    streams: Vec<StreamDecode>,
    plane_source: Vec<(usize, usize)>,
    decorr_v45: Vec<DecorrPass>,
    scratch: UnpackScratch,
    initialized: bool,
    last_flags: i16,
    /// The stream comes from Matroska/WebM (`A_WAVPACK4`): packets are bare WavPack frames
    /// without block headers (see `matroska_block_to_packet`).
    matroska: bool,
    /// The running MD5 of the decoded audio, if the stream's MD5 is to be verified.
    md5: Option<Md5Source>,
    /// The audio hashed by `md5` is the exact original (no lossy hybrid block was decoded).
    md5_valid: bool,
    // Output buffer
    buf: GenericAudioBuffer,
}

impl WavPackDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_WAVPACK {
            return unsupported_error("wavpack decoder: wrong codec id");
        }

        let sample_format = params.sample_format.unwrap_or(SampleFormat::S32);

        // DSD is decoded to the DSD bytes (8 one-bit samples each, most significant bit first), so
        // the output rate is the rate of the bytes.
        let rate = match (params.sample_rate, sample_format) {
            (Some(rate), SampleFormat::U8) => (rate / 8).max(1),
            (rate, _) => rate.unwrap_or(44100),
        };
        let channels = match &params.channels {
            Some(ch) => ch.clone(),
            None => return unsupported_error("wavpack decoder: no channel info"),
        };
        let num_channels = channels.count();
        let spec = AudioSpec::new(rate, channels);
        let buf = GenericAudioBuffer::new(sample_format, spec, 0);

        // Matroska stores a 2-byte version number in CodecPrivate, which is how the demuxer
        // signals that packets are in the Matroska flavour of WavPack blocks.
        let matroska = params.extra_data.as_ref().is_some_and(|extra| extra.len() == 2);

        Ok(WavPackDecoder {
            params: params.clone(),
            dc: DcState::default(),
            decorr: Vec::new(),
            num_terms: 0,
            word_state: WordState::default(),
            v3_samples: Vec::new(),
            streams: Vec::new(),
            plane_source: Vec::new(),
            decorr_v45: Vec::new(),
            scratch: UnpackScratch::default(),
            initialized: false,
            last_flags: 0,
            matroska,
            md5: (opts.verify
                && matches!(params.verification_check, Some(VerificationCheck::Md5(_))))
            .then(|| Md5Source::new(params.extra_data.as_deref(), num_channels)),
            md5_valid: true,
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        // Matroska packets carry no magic; convert them to the internal packet layout first.
        if self.matroska {
            let converted = matroska_block_to_packet(packet.data)?;
            return self.decode_inner_v4v5(&converted);
        }

        // Dispatch on packet type: v4/v5 packets start with "WV45" magic.
        if packet.data.starts_with(PACKET_MAGIC) {
            return self.decode_inner_v4v5(packet.data);
        }

        let data = packet.data;
        let (hdr, audio) = parse_prefix(data)
            .ok_or(symphonia_core::errors::Error::DecodeError("wavpack: packet too short"))?;

        if hdr.total_samples <= 0 {
            self.buf.clear();
            return Ok(());
        }
        // The sample count comes from untrusted input and sizes the output allocation.
        if hdr.total_samples as u32 > MAX_BLOCK_SAMPLES {
            return decode_error("wavpack: block sample count out of range");
        }
        // The precision shift is applied as `<<`/`>>` on 32-bit samples (unpack3.c), so it must
        // be in 0..=31.
        if !(0..=31).contains(&hdr.shift) {
            return decode_error("wavpack: invalid v3 header shift");
        }
        let sample_count = hdr.total_samples as u32;
        let num_channels = hdr.num_channels as u32;

        // The decoder picks the mono or stereo path from `MONO_FLAG`, the output layout from the
        // channel count of the RIFF header. `WavpackOpenFile3()` (unpack3_open.c) rejects a file
        // where the two disagree; so do we, instead of writing to a missing channel plane.
        if ((hdr.flags & MONO_FLAG) != 0) != (num_channels == 1) {
            return decode_error("wavpack v3: mono flag does not match the channel count");
        }

        // First block (or after reset): initialise decorr passes
        if !self.initialized || hdr.flags != self.last_flags {
            unpack_init3(hdr.flags, &mut self.decorr, &mut self.num_terms);
            self.dc = DcState::default();
            self.word_state = WordState::default();
            self.initialized = true;
            self.last_flags = hdr.flags;
        }

        // Decode
        unpack_samples_v3(
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
            &mut self.v3_samples,
        );
        let samples = &self.v3_samples;

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
                    }
                    else {
                        planes[0][idx] = samples[idx * 2].clamp(-32768, 32767) as i16;
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
                    }
                    else {
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
                    }
                    else {
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
                    }
                    else {
                        planes[0][idx] = samples[idx * 2].clamp(-128, 127) as i8;
                        planes[1][idx] = samples[idx * 2 + 1].clamp(-128, 127) as i8;
                    }
                    Ok(())
                })?;
            }
            _ => return unsupported_error("wavpack decoder: unsupported output sample format"),
        }

        // Optional CRC check (version 3 only, lossless)
        if hdr.version == 3 && hdr.bits == 0 && self.dc.crc != hdr.crc as u32 {
            log::warn!("wavpack: CRC mismatch (expected {:08x}, got {:08x})", hdr.crc, self.dc.crc);
        }

        Ok(())
    }

    /// Decode one physical wvpk block (one stream within a possibly-multichannel "block
    /// group"): `main` is the whole block of the `.wv` stream (header and sub-blocks) and
    /// `wvc` the whole matching block of the `.wvc` correction stream, if there is one.
    /// Per-block decorrelation/entropy state (`decorr`/`words`) is local: v4/v5 blocks always
    /// resend their coefficients and medians from scratch, so there is nothing to persist across
    /// calls.
    fn decode_v4v5_stream(
        main: &[u8],
        wvc_block: &[u8],
        decorr: &mut Vec<DecorrPass>,
        scratch: &mut UnpackScratch,
        out: &mut StreamDecode,
    ) -> Result<()> {
        let Some(header) = BlockHeader::parse(main)
        else {
            return decode_error("wavpack v4/v5: stream block too short");
        };

        // `WavpackVerifySingleBlock()`: the block must be well formed and its checksum (if it
        // has one) must match.
        if !verify_block(main) {
            return decode_error("wavpack v4/v5: corrupt block (block checksum mismatch)");
        }

        let flags = header.flags;
        let block_samples = header.block_samples;
        let is_dsd = flags & DSD_FLAG != 0;
        let is_mono = (flags & MONO_DATA) != 0;
        // WavPack's "false stereo" optimization: both channels are identical, so the encoder
        // stores a single decoded channel (FALSE_STEREO set, MONO_FLAG clear) but the track is
        // still reported with two output channels; duplicate the decoded channel into both
        // planes so silence isn't produced on the second channel.
        let is_true_mono = (flags & MONO_FLAG45) != 0;
        let is_false_stereo = flags & FALSE_STEREO != 0 && !is_true_mono;
        let channel_count = if is_true_mono { 1 } else { 2 };

        out.samples.clear();
        out.channel_count = channel_count;
        out.is_false_stereo = is_false_stereo;
        out.block_samples = block_samples;
        out.flags = flags;
        out.lossless = true;

        if block_samples == 0 {
            return Ok(());
        }

        let parts = BlockParts::new(&main[HEADER_LEN..]);

        if is_dsd {
            let Some(dsd_block) = parts.dsd
            else {
                return decode_error("wavpack v4/v5: DSD block without DSD data");
            };

            dsd::unpack_dsd(flags, block_samples, dsd_block, header.crc, &mut out.samples)
                .map_err(Self::unpack_error)?;

            return Ok(());
        }

        // The correction block, if it is usable: a correction block that is corrupt or has no
        // correction bitstream is of no use (and the lossy block is decoded).
        let wvc_header = BlockHeader::parse(wvc_block).filter(|_| verify_block(wvc_block));
        let wvc_parts = wvc_header.map(|_| BlockParts::new(&wvc_block[HEADER_LEN..]));

        let mut words = WordsState45::default();

        if !parse_decorr_terms(parts.terms, decorr) {
            return decode_error("wavpack v4/v5: invalid decorrelation terms");
        }
        parse_decorr_weights(parts.weights, decorr, is_mono);
        parse_decorr_samples(parts.samples, decorr, is_mono);
        parse_entropy_vars(parts.entropy, &mut words, is_mono);
        if (flags & HYBRID_FLAG) != 0 {
            v4v5::parse_hybrid_profile(parts.hybrid, &mut words, is_mono, flags);
        }
        let i32info = parse_int32_info(parts.int32);
        let float_info =
            if (flags & FLOAT_DATA) != 0 { floats::parse_float_info(parts.float) } else { None };

        let main_wvx = WvxInput::parse(parts.wvx, parts.wvx_new);
        let wvc = match (&wvc_header, &wvc_parts) {
            (Some(h), Some(p)) if !p.wvc.is_empty() => Some(WvcInput {
                bits: p.wvc,
                crc: h.crc,
                shaping: p.shaping,
                wvx: WvxInput::parse(p.wvx, p.wvx_new),
            }),
            _ => None,
        };

        out.lossless = unpack_samples_v4v5(
            flags,
            block_samples,
            header.crc,
            decorr,
            &mut words,
            &i32info,
            float_info.as_ref(),
            main_wvx,
            wvc.as_ref(),
            parts.audio,
            scratch,
            &mut out.samples,
        )
        .map_err(Self::unpack_error)?;

        if out.samples.is_empty() {
            return decode_error("wavpack v4/v5: no samples decoded");
        }

        Ok(())
    }

    fn unpack_error(err: UnpackError) -> symphonia_core::errors::Error {
        symphonia_core::errors::Error::DecodeError(match err {
            UnpackError::Malformed => "wavpack v4/v5: malformed or unsupported block",
            UnpackError::Crc => "wavpack v4/v5: CRC mismatch",
        })
    }

    /// Decode a packet built by `WavPackReader::next_packet_v4v5`: one or more per-stream
    /// blocks (a "block group") sharing the same starting sample index, one block per
    /// mono/stereo audio stream. For an ordinary mono/stereo file this is a single
    /// stream; multichannel files (>2 channels) interleave several streams' worth of
    /// samples into the final output exactly like WavPack's own
    /// `unpack_samples_interleave()` does.
    ///
    /// The packet is `"WV45"`, the number of streams (u32 LE), then for each stream the length
    /// (u32 LE) and bytes of its block of the `.wv` stream, and the length and bytes of its block
    /// of the `.wvc` stream (empty if there is none).
    fn decode_inner_v4v5(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 8 {
            return decode_error("wavpack v4/v5: packet too short");
        }
        let stream_count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let mut pos = 8usize;

        // Take the next length-prefixed slice of the packet.
        let mut take = || -> Result<&[u8]> {
            let len_bytes = data.get(pos..pos + 4);
            let len =
                len_bytes.map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or(
                    symphonia_core::errors::Error::DecodeError("wavpack v4/v5: truncated packet"),
                )?;
            let slice = data.get(pos + 4..pos + 4 + len).ok_or(
                symphonia_core::errors::Error::DecodeError("wavpack v4/v5: truncated packet"),
            )?;
            pos += 4 + len;
            Ok(slice)
        };

        // The per-stream buffers persist between packets. `stream_count` is untrusted, but
        // each stream consumes bytes of the packet, so the buffers only grow as far as the
        // packet is long.
        for i in 0..stream_count {
            let main = take()?;
            let wvc = take()?;
            if self.streams.len() <= i {
                self.streams.push(StreamDecode::default());
            }
            Self::decode_v4v5_stream(
                main,
                wvc,
                &mut self.decorr_v45,
                &mut self.scratch,
                &mut self.streams[i],
            )?;
        }

        let streams = &self.streams[..stream_count];

        if streams.is_empty() || streams[0].block_samples == 0 {
            self.buf.clear();
            return Ok(());
        }

        let decoded_frames = streams[0].block_samples as usize;

        // Map each OUTPUT channel (in file order) to (stream index, this stream's local
        // data-channel 0/1), so any channel count (mono/stereo/quad/5.1/7.1/...)
        // interleaves the same way WavPack's own `unpack_samples_interleave()` does.
        let plane_source = &mut self.plane_source;
        plane_source.clear();
        for (i, s) in streams.iter().enumerate() {
            plane_source.push((i, 0));
            if s.channel_count == 2 {
                plane_source.push((i, if s.is_false_stereo { 0 } else { 1 }));
            }
        }
        let plane_source = &self.plane_source;

        // Defensive against malformed/truncated multichannel input (a stream that
        // decoded fewer samples than its declared `block_samples`): read back zero
        // rather than panicking on an out-of-bounds index.
        let sample_at = |stream_idx: usize, local_ch: usize, frame: usize| -> i32 {
            let s = &streams[stream_idx];
            // A false-stereo stream decodes a single data channel (see `StreamDecode`).
            let idx = if s.channel_count == 1 || s.is_false_stereo {
                frame
            }
            else {
                frame * 2 + local_ch
            };
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
            GenericAudioBuffer::U8(b) => {
                // DSD bytes.
                b.grow_capacity(decoded_frames);
                b.render_with(Some(decoded_frames), |idx, planes| {
                    for (plane, &(si, ch)) in plane_source.iter().enumerate() {
                        if let Some(p) = planes.get_mut(plane) {
                            p[idx] = sample_at(si, ch, idx) as u8;
                        }
                    }
                    Ok(())
                })?;
            }
            _ => return unsupported_error("wavpack v4/v5: unsupported output sample format"),
        }

        if let Some(md5) = self.md5.as_mut() {
            if streams.iter().all(|s| s.lossless) {
                // The samples as the file has them, then as the source had them.
                let flags = streams[0].flags;
                md5.samples.clear();
                for idx in 0..decoded_frames {
                    for &(si, ch) in plane_source {
                        md5.samples.push(sample_at(si, ch, idx));
                    }
                }
                md5.update(plane_source.len(), (flags & 3) as usize + 1, flags & DSD_FLAG != 0);
            }
            else {
                self.md5_valid = false;
            }
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
        // The running MD5 is meaningless after a discontinuity (e.g., a seek), so stop verifying
        // rather than report a false mismatch.
        self.md5 = None;
        self.md5_valid = false;
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
        let mut result = FinalizeResult::default();

        // Only the exact original samples have the MD5 of the original.
        if let (Some(md5), Some(VerificationCheck::Md5(expected))) =
            (self.md5.as_mut(), self.params.verification_check)
        {
            if self.md5_valid {
                result.verify_ok = Some(
                    md5.finalize(self.params.channels.as_ref().map_or(0, |c| c.count()))
                        == expected,
                );
            }
        }

        result
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
        &[support_audio_codec!(
            CODEC_ID_WAVPACK,
            "wavpack",
            "WavPack (v1–v5, lossless, hybrid and DSD)"
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO;
    use symphonia_core::packet::Packet;
    use symphonia_core::units::{Duration, Timestamp};

    fn decoder(extra_data: Option<&[u8]>) -> WavPackDecoder {
        let mut params = AudioCodecParameters::new();

        params
            .for_codec(CODEC_ID_WAVPACK)
            .with_sample_rate(44100)
            .with_channels(CHANNEL_LAYOUT_STEREO)
            .with_sample_format(SampleFormat::S16);

        if let Some(extra_data) = extra_data {
            params.with_extra_data(extra_data.into());
        }

        WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
    }

    fn packet(data: Vec<u8>) -> Packet {
        Packet::new(0, Timestamp::new(0), Duration::new(0), data)
    }

    #[test]
    fn verify_reset_discards_the_running_md5() {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_WAVPACK)
            .with_sample_rate(44100)
            .with_channels(CHANNEL_LAYOUT_STEREO)
            .with_sample_format(SampleFormat::S16)
            .with_verification_code(VerificationCheck::Md5([0x55; 16]));
        let opts = AudioDecoderOptions::default().verify(true);

        // Without a seek the (empty) audio is compared with the stream's MD5.
        let mut dec = WavPackDecoder::try_new(&params, &opts).unwrap();
        assert_eq!(dec.finalize().verify_ok, Some(false));

        // After a discontinuity the MD5 of what follows cannot match: no verdict at all.
        let mut dec = WavPackDecoder::try_new(&params, &opts).unwrap();
        dec.reset();
        assert_eq!(dec.finalize().verify_ok, None);
    }

    #[test]
    fn verify_v3_block_with_huge_sample_count_is_rejected() {
        // A 32-byte v3 prefix (version 3, lossless, stereo) that claims the maximum number of
        // samples. This must be rejected rather than used to allocate the output.
        let mut data = vec![0u8; 32];
        data[0..2].copy_from_slice(&3i16.to_le_bytes());
        data[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        data[28..30].copy_from_slice(&2u16.to_le_bytes());
        data[30..32].copy_from_slice(&2u16.to_le_bytes());
        data.extend([0u8; 64]);

        assert!(decoder(None).decode(&packet(data)).is_err());
    }

    #[test]
    fn verify_v3_block_with_out_of_range_shift_is_rejected() {
        for shift in [32i16, 100, -1, i16::MIN] {
            let mut data = vec![0u8; 32];
            data[0..2].copy_from_slice(&3i16.to_le_bytes());
            data[6..8].copy_from_slice(&shift.to_le_bytes());
            data[8..12].copy_from_slice(&16i32.to_le_bytes());
            data[28..30].copy_from_slice(&2u16.to_le_bytes());
            data[30..32].copy_from_slice(&2u16.to_le_bytes());
            data.extend([0u8; 64]);

            assert!(decoder(None).decode(&packet(data)).is_err(), "shift {shift}");
        }
    }

    #[test]
    fn verify_matroska_block_with_huge_sample_count_is_rejected() {
        // A Matroska WavPack block (identified by the 2 byte version number in the codec's extra
        // data) with `u32::MAX` samples.
        let mut data = Vec::new();
        data.extend(u32::MAX.to_le_bytes());
        data.extend(0x1801u32.to_le_bytes());
        data.extend(0u32.to_le_bytes());
        data.extend([0x0a, 0x04, 0, 0, 0, 0, 0, 0, 0, 0]);

        assert!(decoder(Some(&[0x10, 0x04])).decode(&packet(data)).is_err());
    }

    #[test]
    fn verify_garbage_in_any_flavour_is_an_error() {
        // None of these may panic or abort.
        for extra_data in [None, Some(&[0x10u8, 0x04][..])] {
            for len in [0, 1, 3, 4, 8, 11, 12, 13, 31, 32, 33, 100] {
                let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
                let _ = decoder(extra_data).decode(&packet(data));
            }
        }
    }
}
