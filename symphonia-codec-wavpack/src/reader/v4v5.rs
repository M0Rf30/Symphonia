// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WavPack v4/v5 part of the format reader: a stream of `wvpk` blocks, optionally accompanied
//! by a `.wvc` correction stream.
//!
//! Ported from `open_utils.c` (`WavpackOpenFileInputEx64()`, `read_next_header()`,
//! `read_wvc_block()`, `match_wvc_header()`, `seek_eof_information()`) of the WavPack reference
//! implementation (https://github.com/dbry/WavPack, BSD licence).

use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::audio::{Channels, Position, layouts};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, BitOrder, ChannelDataLayout, VerificationCheck,
};
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::*;
use symphonia_core::meta::{MetadataBuilder, MetadataInfo, MetadataLog, well_known};

use crate::block::{
    BYTES_STORED, BlockHeader, FINAL_BLOCK, HAS_CHECKSUM, HEADER_LEN, ID_ALT_MD5_CHECKSUM,
    ID_CHANNEL_INFO, ID_CONFIG_BLOCK, ID_DSD_BLOCK, ID_MD5_CHECKSUM, ID_NEW_CONFIG_BLOCK,
    ID_RIFF_HEADER, ID_RIFF_TRAILER, ID_SAMPLE_RATE, INITIAL_BLOCK, SubBlocks, verify_block,
};

use super::{FormatVersion, SeekIndex, WavPackReader, parse_riff_meta_bytes};
use crate::decoder::md5::encode_extra;
use crate::decoder::{PACKET_MAGIC, dsd_rate_shift};

/// The maximum number of bytes skipped when looking for the next block header (WavPack blocks are
/// smaller than this, so so is any APE tag between them).
const MAX_SKIP: usize = 1024 * 1024;

/// The number of blocks without samples tolerated before the first block with samples.
const MAX_LEADING_BLOCKS: usize = 16;

/// Maximum number of per-stream blocks merged into a single packet (one multichannel
/// "block group"). A generous safety cap (well above any real WavPack file) so a stream that never
/// sets `FINAL_BLOCK` -- malformed input -- can't grow a packet unboundedly.
pub(super) const MAX_STREAMS_PER_PACKET: usize = 512;

/// Read from `reader` until a plausible block header is found, and return it and its bytes.
/// `Ok(None)` is returned at the end of the stream, or if there is no header within `MAX_SKIP`
/// bytes. Port of `read_next_header()` in open_utils.c.
pub(super) fn read_block_header(
    reader: &mut MediaSourceStream<'_>,
) -> Result<Option<(BlockHeader, [u8; HEADER_LEN])>> {
    let mut buf = [0u8; HEADER_LEN];
    let mut have = 0usize;
    let mut skipped = 0usize;

    loop {
        match reader.read_buf_exact(&mut buf[have..]) {
            Ok(()) => (),
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err.into()),
        }

        if BlockHeader::is_plausible(&buf) {
            return Ok(BlockHeader::parse(&buf).map(|header| (header, buf)));
        }

        // Keep the bytes from the next 'w' on.
        let next = buf[1..].iter().position(|&b| b == b'w').map_or(HEADER_LEN, |p| p + 1);
        skipped += next;
        if skipped > MAX_SKIP {
            return Ok(None);
        }

        buf.copy_within(next.., 0);
        have = HEADER_LEN - next;
    }
}

/// Append the rest of the block that starts with `header_bytes` to `out`. The header is
/// appended too. Fails with `UnexpectedEof` if the block is truncated.
fn read_block_into(
    reader: &mut MediaSourceStream<'_>,
    header: &BlockHeader,
    header_bytes: &[u8; HEADER_LEN],
    out: &mut Vec<u8>,
) -> std::io::Result<()> {
    // `ck_size` is below 1 MiB: `read_block_header()` only accepts plausible headers.
    let body_len = (header.ck_size as usize + 8).saturating_sub(HEADER_LEN);
    out.extend_from_slice(header_bytes);
    let start = out.len();
    out.resize(start + body_len, 0);
    reader.read_buf_exact(&mut out[start..])
}

// ---------------------------------------------------------------------------
// `.wvc` correction stream
// ---------------------------------------------------------------------------

/// One block of a `.wvc` correction file.
#[derive(Clone)]
struct WvcBlock {
    header: BlockHeader,
    /// The whole block: header and sub-blocks.
    raw: Vec<u8>,
}

/// The `.wvc` correction stream that accompanies the main (`.wv`) stream.
pub(super) struct Correction<'a> {
    pub(super) reader: MediaSourceStream<'a>,
    /// The position of the first correction block, where a scan restarts after a seek.
    restart_pos: u64,
    /// The next, not yet consumed, correction block.
    pending: Option<WvcBlock>,
    /// The position in the stream where `pending` starts.
    pending_pos: u64,
    /// The end of the correction stream (or an unreadable block) was reached.
    done: bool,
}

/// A saved read position of a `Correction`.
pub(super) struct CorrectionMark {
    pos: u64,
    pending: Option<WvcBlock>,
    pending_pos: u64,
    done: bool,
}

impl<'a> Correction<'a> {
    /// Locate the first block of the correction stream.
    pub(super) fn new(mut reader: MediaSourceStream<'a>) -> Result<Self> {
        reader.ensure_seekback_buffer(HEADER_LEN);
        if read_block_header(&mut reader)?.is_none() {
            return decode_error("wavpack: no blocks in the correction stream");
        }
        reader.seek_buffered_rev(HEADER_LEN);
        let restart_pos = reader.pos();
        Ok(Correction { reader, restart_pos, pending: None, pending_pos: 0, done: false })
    }

    pub(super) fn mark(&self) -> CorrectionMark {
        CorrectionMark {
            pos: self.reader.pos(),
            pending: self.pending.clone(),
            pending_pos: self.pending_pos,
            done: self.done,
        }
    }

    pub(super) fn restore(&mut self, mark: CorrectionMark) -> Result<()> {
        self.reader.seek(SeekFrom::Start(mark.pos))?;
        self.pending = mark.pending;
        self.pending_pos = mark.pending_pos;
        self.done = mark.done;
        Ok(())
    }

    pub(super) fn restart(&mut self) -> Result<()> {
        self.restart_at(self.restart_pos)
    }

    /// Resume reading at `pos`, a position returned by `resume_pos`.
    pub(super) fn restart_at(&mut self, pos: u64) -> Result<()> {
        self.reader.seek(SeekFrom::Start(pos))?;
        self.pending = None;
        self.done = false;
        Ok(())
    }

    /// The position from which reading resumes with the same blocks left to match: the start of
    /// the block read ahead (if any), otherwise the current position.
    pub(super) fn resume_pos(&self) -> u64 {
        if self.pending.is_some() { self.pending_pos } else { self.reader.pos() }
    }

    /// Read the next block of the correction stream.
    fn read_block(&mut self) -> Option<WvcBlock> {
        let (header, header_bytes) = read_block_header(&mut self.reader).ok()??;
        let mut raw = Vec::with_capacity(header.ck_size as usize + 8);
        read_block_into(&mut self.reader, &header, &header_bytes, &mut raw).ok()?;
        Some(WvcBlock { header, raw })
    }

    /// Find the whole correction block that matches the main block with header `wv`, reading
    /// ahead (and discarding stale blocks) as needed. Port of `read_wvc_block()` in open_utils.c.
    /// `None` means that there is no matching block, and the block is decoded lossy.
    pub(super) fn block_for(&mut self, wv: &BlockHeader) -> Option<Vec<u8>> {
        loop {
            if self.pending.is_none() {
                if self.done {
                    return None;
                }
                let start = self.reader.pos();
                match self.read_block() {
                    Some(block) => {
                        self.pending = Some(block);
                        self.pending_pos = start;
                    }
                    None => {
                        self.done = true;
                        return None;
                    }
                }
            }

            let block = self.pending.as_ref()?;
            if block.header.block_samples == 0 {
                self.pending = None;
                continue;
            }

            match match_wvc_header(wv, &block.header) {
                // A match.
                0 => return self.pending.take().map(|block| block.raw),
                // The correction block is from before the main block: skip it.
                1 => self.pending = None,
                // The correction block is for a later block: this one has none.
                _ => return None,
            }
        }
    }
}

/// Compare the header of a main block to that of a potential matching correction block.
/// Port of `match_wvc_header()` in open_utils.c:
///
///   0 = use the correction block,
///   1 = bad match; the correction block is stale, try the next one,
///  -1 = bad match; the correction block is for a later main block.
pub(super) fn match_wvc_header(wv: &BlockHeader, wvc: &BlockHeader) -> i32 {
    if wv.block_index == wvc.block_index && wv.block_samples == wvc.block_samples {
        if wv.flags == wvc.flags {
            return 0;
        }

        let position = |flags: u32| -> i32 {
            let mut p = 0;
            if flags & INITIAL_BLOCK != 0 {
                p -= 1;
            }
            if flags & FINAL_BLOCK != 0 {
                p += 1;
            }
            p
        };

        return if position(wvc.flags) - position(wv.flags) < 0 { 1 } else { -1 };
    }

    // Block indices are 40-bit: a negative 40-bit difference means the correction block is
    // the earlier one.
    if wvc.block_index.wrapping_sub(wv.block_index) & 0x80_0000_0000 != 0 { 1 } else { -1 }
}

// ---------------------------------------------------------------------------
// Stream information (found in the first blocks)
// ---------------------------------------------------------------------------

/// What the leading blocks of a stream say about it.
#[derive(Default)]
struct StreamInfo {
    /// The channel count and speaker mask of `ID_CHANNEL_INFO`.
    channel_info: Option<(u32, u32)>,
    /// The sample rate of `ID_SAMPLE_RATE`.
    sample_rate: Option<u32>,
    /// The power of two of the DSD rate multiplier of `ID_DSD_BLOCK` of the first stream.
    dsd_shift: Option<u32>,
    /// The file has an MD5 checksum (`CONFIG_MD5_CHECKSUM`) and so a block with it at its end.
    has_md5: bool,
    /// The total number of samples of the file, from a block without samples.
    total_hint: Option<u64>,
    /// How the source of the audio differed from WAV audio (`QMODE_*`).
    qmode: u8,
    /// The channel reordering of the source (see `ID_NEW_CONFIG_BLOCK`).
    reorder: Vec<u8>,
}

impl StreamInfo {
    /// Take in the sub-blocks of a block (the bytes after its header).
    fn read_sub_blocks(
        &mut self,
        header: &BlockHeader,
        body: &[u8],
        is_first_stream: bool,
        riff: &mut MetadataBuilder,
    ) {
        for sub_block in SubBlocks::new(body) {
            let data = sub_block.data;

            match sub_block.id {
                ID_CHANNEL_INFO => {
                    self.channel_info = parse_channel_info(data).or(self.channel_info)
                }
                // `read_sample_rate()` in open_utils.c.
                ID_SAMPLE_RATE if data.len() == 3 || data.len() == 4 => {
                    let mut rate = u32::from_le_bytes([data[0], data[1], data[2], 0]);
                    if data.len() == 4 {
                        rate |= u32::from(data[3] & 0x7f) << 24;
                    }
                    self.sample_rate = Some(rate);
                }
                // `read_config_info()` in open_utils.c: the second and third bytes hold the
                // configuration flags bits 16-31 (`CONFIG_MD5_CHECKSUM` is bit 27).
                ID_CONFIG_BLOCK if data.len() >= 3 => {
                    self.has_md5 = data[2] & 0x08 != 0;

                    // An extra mode byte (`CONFIG_EXTRA_MODE` is bit 25) may come before the
                    // low byte of the qmode (written by the 5.0.0 alpha, see `read_config_info()`).
                    let mut rest = &data[3..];
                    if data[2] & 0x02 != 0 && !rest.is_empty() {
                        rest = &rest[1..];
                    }
                    if let Some(&qmode) = rest.first() {
                        self.qmode = qmode;
                    }
                }
                // `read_new_config_info()` in open_utils.c.
                ID_NEW_CONFIG_BLOCK => {
                    self.qmode = 0;
                    self.reorder.clear();

                    if data.len() >= 2 {
                        self.qmode = data[1];
                    }
                    // The channel layout (unused here), its channel count, and the reordering of
                    // the channels, with the channels at the end that are not in it in order.
                    if data.len() > 3 {
                        let nchans = usize::from(data[3]);
                        let order = &data[4..];
                        if order.len() <= nchans {
                            self.reorder = (0..nchans)
                                .map(|i| match order.get(i) {
                                    Some(&c) if usize::from(c) < nchans => c,
                                    Some(_) => 0,
                                    None => i as u8,
                                })
                                .collect();
                        }
                    }
                }
                ID_ALT_MD5_CHECKSUM | ID_MD5_CHECKSUM if data.len() == 16 => self.has_md5 = true,
                ID_DSD_BLOCK if is_first_stream => {
                    self.dsd_shift = dsd_rate_shift(data).or(self.dsd_shift)
                }
                ID_RIFF_HEADER | ID_RIFF_TRAILER => {
                    // Failing to parse the sampler metadata is not fatal.
                    let _ = parse_riff_meta_bytes(data, riff);
                }
                _ => (),
            }
        }

        // `GET_TOTAL_SAMPLES()` of a block without samples and at the start of the file.
        if header.block_samples == 0 && header.block_index == 0 {
            if let Some(total) = header.file_total_samples().filter(|&t| t != 0) {
                self.total_hint.get_or_insert(total);
            }
        }
    }
}

/// Parse the contents of an `ID_CHANNEL_INFO` sub-block: the total channel count and
/// WAVEFORMATEXTENSIBLE-style speaker mask of a multichannel file. Port of `read_channel_info()`
/// in open_utils.c (both the legacy 1-5 byte and the "new" (WavPack 5.0+, `>= 6` byte)
/// unlimited-channel-count encodings).
fn parse_channel_info(data: &[u8]) -> Option<(u32, u32)> {
    let bytecnt = data.len();
    if bytecnt == 0 || bytecnt > 7 {
        return None;
    }

    if bytecnt >= 6 {
        // "New" (2016+) format: a channel-count/max-streams pair (each extended with a nibble of
        // the 3rd byte for >255 channels) followed by a 3 or 4-byte little-endian channel mask.
        let num_channels = (u32::from(data[0]) | (u32::from(data[2] & 0xf) << 8)) + 1;
        let mut mask = u32::from(data[3]) | (u32::from(data[4]) << 8) | (u32::from(data[5]) << 16);
        if bytecnt == 7 {
            mask |= u32::from(data[6]) << 24;
        }
        Some((num_channels, mask))
    }
    else {
        // Legacy format: 1-byte channel count followed by up to 4 mask bytes.
        let mut mask = 0u32;
        for (i, &b) in data[1..].iter().enumerate() {
            mask |= u32::from(b) << (8 * i);
        }
        Some((u32::from(data[0]), mask))
    }
}

/// The channel layout of a file with `num_channels` channels and the speaker mask `mask`.
fn channel_layout(num_channels: u32, mask: u32) -> Channels {
    // The bits of the mask above the 18 speaker positions of WAVEFORMATEXTENSIBLE (which are the
    // ones `Position` has) are for the "unassigned" channels, as are channels beyond the mask.
    let positioned = Position::from_bits_truncate(u64::from(mask));
    if mask & !0x3_ffff == 0 && positioned.bits().count_ones() == num_channels {
        Channels::Positioned(positioned)
    }
    else {
        match num_channels {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            n => Channels::Discrete(n.min(u32::from(u16::MAX)) as u16),
        }
    }
}

/// What a scan to the end of the stream found.
#[derive(Default)]
struct EofInfo {
    /// The index just after the last sample of the last block with samples.
    final_index: Option<u64>,
    /// The MD5 checksum of the audio.
    md5: Option<[u8; 16]>,
}

/// Look at the end of the stream for the index of the final sample and the MD5 checksum, and
/// restore the position. Port of `seek_eof_information()` in open_utils.c.
fn seek_eof_information(reader: &mut MediaSourceStream<'_>, restore_pos: u64) -> Result<EofInfo> {
    let mut info = EofInfo::default();

    let result = scan_eof(reader, &mut info);
    reader.seek(SeekFrom::Start(restore_pos))?;
    result.map(|()| info)
}

fn scan_eof(reader: &mut MediaSourceStream<'_>, info: &mut EofInfo) -> Result<()> {
    let Some(len) = reader.byte_len()
    else {
        return Ok(());
    };

    // Parse the blocks of the last megabyte, and go back another one if there is no audio in
    // them (just blocks of metadata).
    let mut start = len.saturating_sub(MAX_SKIP as u64);
    loop {
        scan_blocks_from(reader, start, info)?;
        if info.final_index.is_some() || start == 0 {
            return Ok(());
        }
        start = start.saturating_sub(MAX_SKIP as u64);
    }
}

/// Parse all of the blocks from `start` to the end of the stream.
fn scan_blocks_from(
    reader: &mut MediaSourceStream<'_>,
    start: u64,
    info: &mut EofInfo,
) -> Result<()> {
    reader.seek(SeekFrom::Start(start))?;

    while let Some((header, header_bytes)) = read_block_header(reader)? {
        if header.block_samples != 0 {
            info.final_index = Some(header.block_index + u64::from(header.block_samples));
        }

        let mut raw = Vec::with_capacity(header.ck_size as usize + 8);
        if read_block_into(reader, &header, &header_bytes, &mut raw).is_err() {
            break;
        }

        for sub_block in SubBlocks::new(&raw[HEADER_LEN..]) {
            if matches!(sub_block.id, ID_MD5_CHECKSUM | ID_ALT_MD5_CHECKSUM)
                && sub_block.data.len() == 16
            {
                let mut md5 = [0u8; 16];
                md5.copy_from_slice(sub_block.data);
                info.md5 = Some(md5);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl<'s> WavPackReader<'s> {
    pub(super) fn try_new_v4v5(
        mut mss: MediaSourceStream<'s>,
        original_pos: u64,
        mut metadata: MetadataLog,
    ) -> Result<Self> {
        mss.ensure_seekback_buffer(HEADER_LEN);

        let mut riff = MetadataBuilder::new(MetadataInfo {
            metadata: well_known::METADATA_ID_WAVE,
            short_name: "riff",
            long_name: "RIFF/WAVE Sampler Metadata",
        });
        let mut info = StreamInfo::default();

        // Read blocks until one with samples is found: blocks without samples (carrying, say,
        // the wrapper or the configuration) may come first. Blocks that are corrupt are skipped.
        let mut restart_pos = None;
        let mut leading = 0usize;
        let first = loop {
            let Some((header, header_bytes)) = read_block_header(&mut mss)?
            else {
                return decode_error("wavpack: no blocks found");
            };

            restart_pos.get_or_insert(mss.pos() - HEADER_LEN as u64);

            let mut raw = Vec::with_capacity(header.ck_size as usize + 8);
            if read_block_into(&mut mss, &header, &header_bytes, &mut raw).is_err() {
                return decode_error("wavpack: can't read all of the first block");
            }

            if !verify_block(&raw) {
                // Skipped, like the reference does, but only so many of them.
                leading += 1;
                if leading > MAX_LEADING_BLOCKS {
                    return decode_error("wavpack: not compatible with this version of WavPack");
                }
                continue;
            }

            info.read_sub_blocks(&header, &raw[HEADER_LEN..], true, &mut riff);

            if header.block_samples != 0 {
                break (header, raw);
            }

            leading += 1;
            if leading > MAX_LEADING_BLOCKS {
                return decode_error("wavpack: not compatible with this version of WavPack");
            }
        };
        let (header, _) = first;
        let restart_pos = restart_pos.unwrap_or(original_pos);

        // The remaining blocks of the first group (one per stream) tell the channel count if
        // the stream does not.
        let mut group_channels = if header.is_mono() { 1u32 } else { 2 };
        if info.channel_info.is_none() && header.flags & FINAL_BLOCK == 0 {
            for _ in 1..MAX_STREAMS_PER_PACKET {
                let Some((next, next_bytes)) = read_block_header(&mut mss)?
                else {
                    break;
                };
                if next.block_index != header.block_index || next.block_samples == 0 {
                    break;
                }
                let mut raw = Vec::with_capacity(next.ck_size as usize + 8);
                if read_block_into(&mut mss, &next, &next_bytes, &mut raw).is_err() {
                    break;
                }
                group_channels += if next.is_mono() { 1 } else { 2 };
                if next.flags & FINAL_BLOCK != 0 {
                    break;
                }
            }
        }

        // The total number of samples, and where the timeline of the stream starts. A stream
        // that does not start at sample 0 (say, cut from a longer one) or does not say how long
        // it is gets those from its end, if the source can be seeked.
        let mut initial_index = 0u64;
        let mut total_samples = info.total_hint;
        let mut md5 = None;

        let from_end = header.block_index != 0 || header.file_total_samples().is_none();
        if from_end {
            initial_index = header.block_index;
            total_samples = None;
        }
        else {
            total_samples = header.file_total_samples().or(total_samples);
        }

        if (from_end || info.has_md5) && mss.is_seekable() {
            let here = mss.pos();
            let eof = seek_eof_information(&mut mss, here)?;
            if from_end {
                total_samples =
                    eof.final_index.map(|final_index| final_index.saturating_sub(initial_index));
            }
            md5 = eof.md5.filter(|_| info.has_md5);
        }

        // The audio parameters.
        let is_dsd = header.is_dsd();
        let mut codec_params = AudioCodecParameters::new();
        codec_params.for_codec(CODEC_ID_WAVPACK);

        // The sample rate of the header: a standard one, or 44100 if the header refers to
        // `ID_SAMPLE_RATE` and there is none.
        let rate = info.sample_rate.or(header.standard_sample_rate()).unwrap_or(44100);

        let ts_scale = if is_dsd {
            // The sample rate of the header is that of the DSD bytes divided by a power of two,
            // and DSD rates are given in bits: 8 bits per byte.
            let Some(shift) = info.dsd_shift
            else {
                return decode_error("wavpack: DSD block without DSD data");
            };
            let Some(rate) = rate
                .checked_mul(8)
                .and_then(|rate| rate.checked_shl(shift))
                .filter(|&native| native >> shift == rate * 8 && native != 0)
            else {
                return decode_error("wavpack: DSD sample rate out of range");
            };

            codec_params
                .with_sample_rate(rate)
                .with_bits_per_sample(1)
                .with_bits_per_coded_sample(1)
                .with_sample_format(SampleFormat::U8)
                .with_bit_order(BitOrder::MsbFirst)
                .with_channel_data_layout(ChannelDataLayout::Interleaved);
            8u64
        }
        else {
            let bytes_per_sample = (header.flags & BYTES_STORED) + 1;
            let sample_format = if header.is_float() {
                SampleFormat::F32
            }
            else {
                match bytes_per_sample {
                    1 => SampleFormat::S8,
                    2 => SampleFormat::S16,
                    3 => SampleFormat::S24,
                    _ => SampleFormat::S32,
                }
            };

            codec_params
                .with_sample_rate(rate)
                .with_bits_per_coded_sample(bytes_per_sample * 8)
                .with_bits_per_sample(if header.is_float() { 32 } else { header.bits_per_sample() })
                .with_sample_format(sample_format);
            1
        };

        let channels = match info.channel_info {
            Some((n, mask)) if n > 0 => channel_layout(n, mask),
            // Without channel information the channels are those of the first group of blocks.
            _ if group_channels == 1 => layouts::CHANNEL_LAYOUT_MONO,
            _ if group_channels == 2 => layouts::CHANNEL_LAYOUT_STEREO,
            _ => Channels::Discrete(group_channels.min(u32::from(u16::MAX)) as u16),
        };
        codec_params.with_channels(channels);

        if let Some(md5) = md5 {
            codec_params.with_verification_code(VerificationCheck::Md5(md5));
            codec_params.with_extra_data(encode_extra(info.qmode, &info.reorder));
        }

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        if let Some(total) = total_samples {
            let total = total.saturating_mul(ts_scale);
            track.with_num_frames(total);
            track.with_duration(Duration::new(total));
        }

        // GrandOrgue / Hauptwerk store loop points (`smpl`) and the release marker (`cue `) inside
        // the WAV chunks that WavPack preserves in the RIFF header.
        let revision = riff.build();
        if !revision.media.tags.is_empty() {
            metadata.push(revision);
        }

        mss.seek(SeekFrom::Start(restart_pos))?;
        let media_info = MediaInfo::from_track(&track);

        Ok(WavPackReader {
            reader: mss,
            media_info,
            tracks: vec![track],
            metadata,
            chapters: None,
            next_packet_ts: 0,
            restart_pos,
            index: SeekIndex::new(),
            format_version: FormatVersion::V4V5,
            correction: None,
            initial_index,
            ts_scale,
            started: false,
        })
    }

    /// Read the next packet of a v4/v5 stream: a "block group", that is one or more consecutive
    /// wvpk blocks sharing the same starting sample index, one block per audio stream (a mono or
    /// stereo pair) covering all of the file's channels between them (`INITIAL_BLOCK` on the
    /// first, `FINAL_BLOCK` on the last). For an ordinary mono/stereo file (a single stream) every
    /// block has both flags set, so a packet is a block.
    ///
    /// The packet is `"WV45"`, the number of streams (u32 LE), then for each stream the length
    /// (u32 LE) and bytes of its block of the `.wv` stream, and the length and bytes of its block
    /// of the `.wvc` stream (empty if there is none).
    ///
    /// Blocks without samples (a trailing wrapper or tag block) are skipped. A packet is made of
    /// the blocks as they are in the file: they are verified and decoded by the decoder.
    pub(super) fn next_packet_v4v5(&mut self) -> Result<Option<Packet>> {
        let mut pkt: Vec<u8> = Vec::new();
        pkt.extend_from_slice(PACKET_MAGIC);
        pkt.extend_from_slice(&[0; 4]);

        let mut streams = 0u32;
        let mut first: Option<BlockHeader> = None;
        let mut complete = true;

        while let Some((header, header_bytes)) = read_block_header(&mut self.reader)? {
            // A block without samples has no audio (nor correction block).
            if header.block_samples == 0 {
                let skip = (header.ck_size as u64 + 8).saturating_sub(HEADER_LEN as u64);
                if self.reader.ignore_bytes(skip).is_err() {
                    complete = false;
                    break;
                }
                continue;
            }

            match &first {
                Some(first) => {
                    // A block of another group: the rest of this group is missing.
                    if header.block_index != first.block_index
                        || header.block_samples != first.block_samples
                    {
                        self.reader.seek_buffered_rev(HEADER_LEN);
                        break;
                    }
                }
                None => first = Some(header),
            }

            let len_pos = pkt.len();
            pkt.extend_from_slice(&[0; 4]);
            if read_block_into(&mut self.reader, &header, &header_bytes, &mut pkt).is_err() {
                // A truncated block.
                complete = false;
                break;
            }
            let len = (pkt.len() - len_pos - 4) as u32;
            pkt[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());

            let wvc = self.correction.as_mut().and_then(|c| c.block_for(&header));
            match wvc {
                Some(wvc) => {
                    pkt.extend_from_slice(&(wvc.len() as u32).to_le_bytes());
                    pkt.extend_from_slice(&wvc);
                }
                None => pkt.extend_from_slice(&[0; 4]),
            }

            streams += 1;
            if header.flags & FINAL_BLOCK != 0 || streams as usize >= MAX_STREAMS_PER_PACKET {
                break;
            }
        }

        // A truncated block ends the stream, and with it a group that is incomplete.
        let Some(first) = first.filter(|_| streams > 0 && complete)
        else {
            return Ok(None);
        };
        pkt[4..8].copy_from_slice(&streams.to_le_bytes());

        let scale = self.ts_scale;
        let ts = first.block_index.saturating_sub(self.initial_index).saturating_mul(scale);
        let dur = u64::from(first.block_samples) * scale;
        self.next_packet_ts = ts.saturating_add(dur).min(i64::MAX as u64) as i64;

        Ok(Some(Packet::new(
            0,
            Timestamp::new(ts.min(i64::MAX as u64) as i64),
            Duration::new(dur),
            pkt,
        )))
    }
}

// ---------------------------------------------------------------------------
// Matroska
// ---------------------------------------------------------------------------

/// Convert one Matroska/WebM `A_WAVPACK4` block into the internal `WV45` packet the decoder
/// works with (the same one `WavPackReader` produces for native `.wv` files).
///
/// Matroska stores WavPack blocks with the 32-byte `wvpk` header stripped down to just the
/// fields that vary per block. The layout of a frame is:
///
///   block_samples(4)                                 -- once per frame
///   { flags(4) crc(4) [block_size(4)] payload }...   -- one entry per WavPack block
///
/// where `block_size` is only present when the block is not a single-block (mono/stereo)
/// stream, i.e. when the block is not both `INITIAL_BLOCK` and `FINAL_BLOCK`; otherwise the
/// payload runs to the end of the frame. The payload is the sequence of WavPack sub-blocks.
/// The block header is rebuilt from these fields (without a block checksum, which would not
/// match the rebuilt header).
pub(crate) fn matroska_block_to_packet(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(data);

    let block_samples = reader.read_u32()?;

    let mut pkt: Vec<u8> = Vec::new();
    pkt.extend_from_slice(PACKET_MAGIC);
    pkt.extend_from_slice(&[0; 4]);

    let mut streams = 0u32;

    while (reader.pos() as usize) < data.len() && (streams as usize) < MAX_STREAMS_PER_PACKET {
        let flags = reader.read_u32()?;
        let crc = reader.read_u32()?;

        let remaining = data.len() - reader.pos() as usize;
        let single_block = flags & (INITIAL_BLOCK | FINAL_BLOCK) == (INITIAL_BLOCK | FINAL_BLOCK);

        let size = if single_block { remaining } else { reader.read_u32()? as usize };

        let start = reader.pos() as usize;
        if size > data.len() - start {
            return decode_error("wavpack: matroska block size exceeds frame");
        }
        let payload = &data[start..start + size];
        if !SubBlocks::new(payload).is_well_formed() {
            return decode_error("wavpack: malformed matroska block");
        }

        // The block: a rebuilt header followed by the sub-blocks.
        let flags = flags & !HAS_CHECKSUM;
        pkt.extend_from_slice(&((HEADER_LEN + size) as u32).to_le_bytes());
        pkt.extend_from_slice(b"wvpk");
        pkt.extend_from_slice(&((24 + size) as u32).to_le_bytes());
        pkt.extend_from_slice(&0x410u16.to_le_bytes());
        pkt.extend_from_slice(&[0; 2]); // block_index_u8, total_samples_u8
        pkt.extend_from_slice(&0u32.to_le_bytes()); // total_samples
        pkt.extend_from_slice(&0u32.to_le_bytes()); // block_index
        pkt.extend_from_slice(&block_samples.to_le_bytes());
        pkt.extend_from_slice(&flags.to_le_bytes());
        pkt.extend_from_slice(&crc.to_le_bytes());
        pkt.extend_from_slice(payload);
        // No correction block.
        pkt.extend_from_slice(&[0; 4]);

        reader.ignore_bytes(size as u64)?;
        streams += 1;

        if flags & FINAL_BLOCK != 0 {
            break;
        }
    }

    if streams == 0 {
        return decode_error("wavpack: empty matroska block");
    }

    pkt[4..8].copy_from_slice(&streams.to_le_bytes());
    Ok(pkt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// A WavPack sub-block with an even number of data bytes.
    fn sub_block(id: u8, data: &[u8]) -> Vec<u8> {
        assert!(data.len() % 2 == 0 && data.len() < 512);
        let mut v = vec![id, (data.len() / 2) as u8];
        v.extend(data);
        v
    }

    fn wv_header(block_index: u64, block_samples: u32, flags: u32) -> BlockHeader {
        BlockHeader {
            ck_size: 24,
            block_index,
            total_samples: 0,
            total_samples_u8: 0,
            block_samples,
            flags,
            crc: 0,
        }
    }

    #[test]
    fn verify_matroska_single_block_gets_a_header() {
        let payload = sub_block(0x02, &[1, 2, 3, 4]);
        let flags = INITIAL_BLOCK | FINAL_BLOCK | 1;

        let mut frame = Vec::new();
        frame.extend(le(1000));
        frame.extend(le(flags));
        frame.extend(le(0xdead_beef));
        frame.extend(&payload);

        let pkt = matroska_block_to_packet(&frame).unwrap();
        assert_eq!(&pkt[..4], PACKET_MAGIC);
        assert_eq!(pkt[4..8], le(1));

        // The stream: its length, the block, and the empty correction stream.
        let len = u32::from_le_bytes(pkt[8..12].try_into().unwrap()) as usize;
        assert_eq!(len, HEADER_LEN + payload.len());
        let block = &pkt[12..12 + len];
        let header = BlockHeader::parse(block[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!((header.block_samples, header.flags, header.crc), (1000, flags, 0xdead_beef));
        assert_eq!(&block[HEADER_LEN..], &payload[..]);
        assert_eq!(pkt[12 + len..], le(0));
    }

    #[test]
    fn verify_matroska_multichannel_block_has_a_size_per_block() {
        let first = sub_block(0x02, &[1, 2, 3, 4]);
        let second = sub_block(0x03, &[5, 6]);

        let mut frame = Vec::new();
        frame.extend(le(64));
        frame.extend(le(INITIAL_BLOCK));
        frame.extend(le(1));
        frame.extend(le(first.len() as u32));
        frame.extend(&first);
        frame.extend(le(FINAL_BLOCK));
        frame.extend(le(2));
        frame.extend(le(second.len() as u32));
        frame.extend(&second);

        let pkt = matroska_block_to_packet(&frame).unwrap();
        assert_eq!(pkt[4..8], le(2));

        let len = u32::from_le_bytes(pkt[8..12].try_into().unwrap()) as usize;
        assert_eq!(len, HEADER_LEN + first.len());
        let next = 12 + len + 4;
        let len2 = u32::from_le_bytes(pkt[next..next + 4].try_into().unwrap()) as usize;
        assert_eq!(len2, HEADER_LEN + second.len());
    }

    #[test]
    fn verify_malformed_matroska_blocks_are_errors() {
        // Nothing, or only the sample count.
        assert!(matroska_block_to_packet(&[]).is_err());
        assert!(matroska_block_to_packet(&le(100)).is_err());

        // A block that is larger than the frame.
        let mut frame = Vec::new();
        frame.extend(le(64));
        frame.extend(le(INITIAL_BLOCK));
        frame.extend(le(0));
        frame.extend(le(1000));
        frame.extend([0u8; 8]);
        assert!(matroska_block_to_packet(&frame).is_err());

        // Sub-blocks that do not add up to the size of the block.
        let mut frame = Vec::new();
        frame.extend(le(64));
        frame.extend(le(INITIAL_BLOCK | FINAL_BLOCK));
        frame.extend(le(0));
        frame.extend([0x02, 0x08, 1, 2]);
        assert!(matroska_block_to_packet(&frame).is_err());
    }

    #[test]
    fn verify_correction_block_matching() {
        let flags = INITIAL_BLOCK | FINAL_BLOCK;
        let wv = wv_header(4096, 1024, flags);

        // The same block.
        assert_eq!(match_wvc_header(&wv, &wv_header(4096, 1024, flags)), 0);
        // A correction block of a later block means the main block has no correction, and one of
        // an earlier block is stale.
        assert_eq!(match_wvc_header(&wv, &wv_header(5120, 1024, flags)), -1);
        assert_eq!(match_wvc_header(&wv, &wv_header(1024, 1024, flags)), 1);
        // The same samples, but the correction block is of another channel of the group.
        assert_eq!(
            match_wvc_header(&wv_header(0, 10, INITIAL_BLOCK), &wv_header(0, 10, FINAL_BLOCK)),
            -1
        );
        assert_eq!(
            match_wvc_header(&wv_header(0, 10, FINAL_BLOCK), &wv_header(0, 10, INITIAL_BLOCK)),
            1
        );
        // The distance is that of 40-bit block indices.
        assert_eq!(
            match_wvc_header(&wv_header(0, 10, flags), &wv_header((1 << 40) - 10, 10, flags)),
            1
        );
    }

    #[test]
    fn verify_stream_info_reads_the_configuration() {
        let mut riff = MetadataBuilder::new(MetadataInfo {
            metadata: well_known::METADATA_ID_WAVE,
            short_name: "riff",
            long_name: "RIFF/WAVE Sampler Metadata",
        });
        let header = wv_header(0, 10, INITIAL_BLOCK | FINAL_BLOCK);

        // The new configuration: the file format, the qmode (reordered channels), the channel
        // layout, the number of channels, and the order, which is padded with the identity.
        let mut info = StreamInfo::default();
        let body = sub_block(ID_NEW_CONFIG_BLOCK, &[0, 0x08, 0, 4, 2, 0]);
        info.read_sub_blocks(&header, &body, true, &mut riff);
        assert_eq!(info.qmode, 0x08);
        assert_eq!(info.reorder, [2, 0, 2, 3]);

        // The old configuration: flags, with an MD5, and the qmode after the extra mode byte.
        let mut info = StreamInfo::default();
        let body = sub_block(ID_CONFIG_BLOCK, &[0, 0, 0x0a, 0x55, 0x04, 0]);
        info.read_sub_blocks(&header, &body, true, &mut riff);
        assert!(info.has_md5);
        assert_eq!(info.qmode, 0x04);
    }
}
