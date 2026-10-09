// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia_core::support_format;

use symphonia_core::errors::{decode_error, seek_error, unsupported_error, Error, Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_WAVPACK;
use symphonia_core::formats::FormatReader;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataInfo, MetadataLog, Tag, well_known,
};
use symphonia_core::audio::layouts;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::audio::sample::SampleFormat;

use log::debug;

use crate::decoder::{EXT_WVC_WVX_NEW, EXT_WVX_NEW, STREAM_HDR};

mod sub_block;
use sub_block::{decode_sub_block, Encoding, SubBlock};

const WAVPACK_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_WAVPACK,
    short_name: "wavpack",
    long_name: "WavPack",
};

const STREAM_MARKER: [u8; 4] = *b"wvpk";
const RIFF_MARKER:   [u8; 4] = *b"RIFF";
const WAVE_MARKER:   [u8; 4] = *b"WAVE";

const SAMPLE_RATES: [u32; 15] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100,
    48000, 64000, 88200, 96000, 192000,
];

macro_rules! combine_values {
    ($u32_value:expr, $u8_value:expr) => {
        (($u8_value as u64) << 32) | ($u32_value as u64)
    };
}

// ---------------------------------------------------------------------------
// Internal format version discriminant
// ---------------------------------------------------------------------------

enum FormatVersion {
    /// WavPack v1–v3: raw PCM samples wrapped in a RIFF/WAVE container.
    V3 { num_channels: u16, bytes_per_sample: u16 },
    /// WavPack v4/v5: native wvpk block stream.
    V4V5,
}

// ---------------------------------------------------------------------------
// Reader struct
// ---------------------------------------------------------------------------

/// Format reader for WavPack (v1–v3 RIFF wrapper and v4/v5 native streams).
pub struct WavPackReader<'a> {
    reader: MediaSourceStream<'a>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    /// Tracks the next packet's presentation timestamp.
    /// Uses i64 to match the signed `Timestamp` newtype introduced in dev-0.6.
    next_packet_ts: i64,
    /// Byte offset of the first WavPack block, used to restart a linear scan on `seek`.
    restart_pos: u64,
    format_version: FormatVersion,
    /// The `.wvc` correction stream of a hybrid file, if one is attached.
    correction: Option<Correction<'a>>,
    /// Sparse, bounded index of the packet positions seen so far, built as packets are read.
    index: SeekIndex,
}

/// The maximum number of entries of a `SeekIndex`.
const MAX_INDEX_ENTRIES: usize = 4096;

/// A packet position: the timestamp and the stream positions to resume reading from.
#[derive(Clone, Copy)]
struct IndexEntry {
    ts: i64,
    /// The position in the main stream.
    pos: u64,
    /// The position in the correction stream (0 if there is none).
    wvc_pos: u64,
}

/// A sparse index of packet positions in ascending timestamp order, so a seek does not have to
/// rescan the stream from its start. Entries are added while packets are read (including during
/// the scan of a seek, which extends the index towards the target). The memory is bounded: when
/// `MAX_INDEX_ENTRIES` is reached every other entry is dropped and the spacing is doubled.
struct SeekIndex {
    entries: Vec<IndexEntry>,
    /// The minimum timestamp distance between two entries.
    min_gap: i64,
}

impl SeekIndex {
    fn new() -> Self {
        SeekIndex { entries: Vec::new(), min_gap: 1 }
    }

    /// Returns `true` if a packet starting at `ts` should be indexed.
    fn wants(&self, ts: i64) -> bool {
        match self.entries.last() {
            Some(last) => ts >= last.ts.saturating_add(self.min_gap),
            None => true,
        }
    }

    fn insert(&mut self, entry: IndexEntry) {
        if !self.wants(entry.ts) {
            return;
        }
        if self.entries.len() >= MAX_INDEX_ENTRIES {
            let span = entry.ts - self.entries[0].ts;
            let avg_gap = span / self.entries.len() as i64;
            let mut keep = false;
            self.entries.retain(|_| {
                keep = !keep;
                keep
            });
            self.min_gap = self.min_gap.saturating_mul(2).max(avg_gap.saturating_mul(2));
            // The spacing may have grown beyond the new entry.
            if !self.wants(entry.ts) {
                return;
            }
        }
        self.entries.push(entry);
    }

    /// The last entry that starts at or before `ts`.
    fn find(&self, ts: i64) -> Option<IndexEntry> {
        let n = self.entries.partition_point(|e| e.ts <= ts);
        n.checked_sub(1).map(|i| self.entries[i])
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl<'s> WavPackReader<'s> {
    /// Create a reader for the WavPack stream `mss`.
    ///
    /// If `opts` carries a sidecar source (`FormatOptions::sidecar`), it is used as the `.wvc`
    /// correction stream of a hybrid file, which makes the decoder produce the bit-exact
    /// lossless audio. A sidecar that is not a valid correction stream is ignored (the lossy
    /// audio is decoded) and so is one provided for a v1-v3 file.
    pub fn try_new(mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        let sidecar = opts.external_data.sidecar.take().and_then(|sidecar| sidecar.take());

        let mut reader = Self::try_new_main(mss, opts)?;

        if let Some(sidecar) = sidecar {
            if let Err(err) = reader.attach_correction(MediaSourceStream::new(sidecar, Default::default())) {
                log::warn!("wavpack: ignoring the correction stream: {}", err);
            }
        }

        Ok(reader)
    }

    /// Create a reader for the hybrid WavPack stream `mss` (a `.wv` file) with its `.wvc`
    /// correction stream `wvc`, which makes the decoder produce the bit-exact lossless audio.
    ///
    /// Unlike [`try_new`](Self::try_new), an unusable correction stream is an error.
    pub fn try_new_with_correction(
        mss: MediaSourceStream<'s>,
        wvc: MediaSourceStream<'s>,
        mut opts: FormatOptions,
    ) -> Result<Self> {
        // The explicitly provided stream takes precedence over a sidecar.
        opts.external_data.sidecar = None;

        let mut reader = Self::try_new_main(mss, opts)?;
        reader.attach_correction(wvc)?;
        Ok(reader)
    }

    /// Returns `true` if a `.wvc` correction stream is attached to this reader.
    pub fn has_correction(&self) -> bool {
        self.correction.is_some()
    }

    /// Attach the `.wvc` correction stream `wvc`. Only valid for WavPack v4/v5 streams, and
    /// only before the first packet is read.
    fn attach_correction(&mut self, wvc: MediaSourceStream<'s>) -> Result<()> {
        if !matches!(self.format_version, FormatVersion::V4V5) {
            return unsupported_error("wavpack: correction streams need a v4/v5 stream");
        }
        if self.next_packet_ts != 0 {
            return unsupported_error("wavpack: the correction stream must be attached up front");
        }

        match Correction::new(wvc) {
            Ok(correction) => {
                self.correction = Some(correction);
                Ok(())
            }
            Err(_) => decode_error("wavpack: no blocks in the correction stream"),
        }
    }

    fn try_new_main(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        let original_pos = mss.pos();
        let magic = mss.read_quad_bytes()?;
        mss.seek(std::io::SeekFrom::Start(original_pos))?;

        // Metadata the probe already read (APEv2 and ID3v2 blocks, which is where WavPack
        // files actually carry their tags) must be carried into the reader's log, otherwise
        // every tag is silently dropped.
        let external = opts.external_data.metadata.take().unwrap_or_default();

        if magic == RIFF_MARKER {
            Self::try_new_v3(mss, external)
        } else {
            Self::try_new_v4v5(mss, original_pos, external)
        }
    }

    // ------------------------------------------------------------------
    // WavPack v1–v3: RIFF/WAVE wrapper
    // ------------------------------------------------------------------

    fn try_new_v3(mut mss: MediaSourceStream<'s>, mut metadata: MetadataLog) -> Result<Self> {
        let riff_id = mss.read_quad_bytes()?;
        if riff_id != RIFF_MARKER {
            return decode_error("wavpack v3: expected RIFF");
        }
        let _riff_size = mss.read_u32()?;
        let wave_id = mss.read_quad_bytes()?;
        if wave_id != WAVE_MARKER {
            return decode_error("wavpack v3: expected WAVE");
        }

        let mut wav_header: Option<WaveHeader3> = None;
        let mut meta_builder = MetadataBuilder::new(MetadataInfo {
            metadata: well_known::METADATA_ID_WAVE,
            short_name: "riff",
            long_name: "RIFF/WAVE Sampler Metadata",
        });

        loop {
            let chunk_id   = mss.read_quad_bytes()?;
            let chunk_size = mss.read_u32()?;

            match &chunk_id {
                b"fmt " => {
                    if chunk_size < 16 {
                        return decode_error("wavpack v3: fmt chunk too small");
                    }
                    let format_tag      = mss.read_u16()?;
                    let num_channels    = mss.read_u16()?;
                    let sample_rate     = mss.read_u32()?;
                    let _bytes_per_sec  = mss.read_u32()?;
                    let block_align     = mss.read_u16()?;
                    let bits_per_sample = mss.read_u16()?;

                    let extra = chunk_size - 16;
                    if extra > 0 { mss.ignore_bytes(extra as u64)?; }
                    if chunk_size % 2 != 0 { let _ = mss.read_u8()?; }

                    if format_tag != 1 {
                        return unsupported_error("wavpack v3: non-PCM fmt");
                    }
                    if num_channels == 0 || num_channels > 2 {
                        return decode_error("wavpack v3: unsupported channel count");
                    }
                    let bytes_per_sample = block_align / num_channels;
                    wav_header = Some(WaveHeader3 {
                        sample_rate, num_channels, bits_per_sample, bytes_per_sample,
                    });
                }

                b"smpl" => {
                    parse_smpl_chunk(&mut mss, chunk_size, &mut meta_builder)?;
                    if chunk_size % 2 != 0 { let _ = mss.read_u8()?; }
                }

                b"cue " => {
                    parse_cue_chunk(&mut mss, chunk_size, &mut meta_builder)?;
                    if chunk_size % 2 != 0 { let _ = mss.read_u8()?; }
                }

                b"data" => {
                    // WavPack blocks start here.
                    break;
                }

                _ => {
                    let skip = chunk_size + (chunk_size % 2);
                    mss.ignore_bytes(skip as u64)?;
                }
            }
        }

        let wav = match wav_header {
            Some(w) => w,
            None => return decode_error("wavpack v3: no fmt chunk"),
        };

        let channel_layout = if wav.num_channels == 1 {
            layouts::CHANNEL_LAYOUT_MONO
        } else {
            layouts::CHANNEL_LAYOUT_STEREO
        };

        let bits_per_sample  = wav.bits_per_sample  as u32;
        let bytes_per_sample = wav.bytes_per_sample as u32;

        if bytes_per_sample == 0 || bytes_per_sample > 4 {
            return decode_error("wavpack v3: unsupported bytes per sample");
        }

        // The WavPackDecoder always outputs i32; sample_format matches the bit depth
        // so downstream consumers know the effective range.
        let sample_format = match bytes_per_sample {
            1 => SampleFormat::S8,
            2 => SampleFormat::S16,
            3 => SampleFormat::S24,
            _ => SampleFormat::S32,
        };

        let mut codec_params = AudioCodecParameters::new();
        codec_params
            .for_codec(CODEC_ID_WAVPACK)
            .with_bits_per_coded_sample(bits_per_sample)
            .with_bits_per_sample(bits_per_sample)
            .with_channels(channel_layout)
            .with_sample_rate(wav.sample_rate)
            .with_sample_format(sample_format);

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        // data_start_pos is where the first "wvpk" block begins; next_packet_v3 resumes
        // reading from here. Pre-scan the block headers (cheap: 32 bytes each) to find the
        // track's exact total sample count, which the RIFF/WAVE wrapper does not state
        // anywhere else.
        let data_start_pos = mss.pos();
        if let Some(total_samples) = scan_v3_total_samples(&mut mss, data_start_pos) {
            track.with_num_frames(total_samples);
            track.with_duration(Duration::new(total_samples));
        }

        metadata.push(meta_builder.build());

        let media_info = MediaInfo::from_track(&track);

        Ok(WavPackReader {
            reader: mss,
            media_info,
            tracks: vec![track],
            metadata,
            chapters: None,
            next_packet_ts: 0,
            restart_pos: data_start_pos,
            index: SeekIndex::new(),
            format_version: FormatVersion::V3 {
                num_channels: wav.num_channels,
                bytes_per_sample: wav.bytes_per_sample,
            },
            correction: None,
        })
    }

    // ------------------------------------------------------------------
    // WavPack v4/v5: native wvpk block stream
    // ------------------------------------------------------------------

    fn try_new_v4v5(
        mut mss: MediaSourceStream<'s>,
        original_pos: u64,
        mut metadata: MetadataLog,
    ) -> Result<Self> {
        let _ = find_next_block(&mut mss, 100);
        let header_pos = mss.pos();
        let header = Header::decode(&mut mss)?;
        if header.get_block_index() != 0 {
            debug!("First block is not first block after all.");
        }

        // Read the rest of the first block's sub-blocks once, up front: used both to
        // find `ID_CHANNEL_INFO` (multichannel layout, below) and to scan for RIFF/WAVE
        // sampler metadata (`smpl`/`cue` chunks) further down. `ck_size` counts
        // everything after the `ck_size` field itself; the remaining 24 bytes of the
        // 32-byte header have already been consumed by `Header::decode`.
        let sub_blocks_len = (header.ck_size as u64).saturating_sub(24);
        // `ck_size` is untrusted: read incrementally (the allocation is bounded by the bytes
        // actually present in the stream) rather than pre-allocating `ck_size` bytes.
        let sub_buf = match usize::try_from(sub_blocks_len)
            .ok()
            .and_then(|len| mss.read_boxed_slice_exact(len).ok())
        {
            Some(buf) => buf.into_vec(),
            None => Vec::new(),
        };
        let have_sub_buf = sub_buf.len() as u64 == sub_blocks_len;

        // `ID_CHANNEL_INFO` (present whenever the file has more than 2 channels, or
        // channels that don't map to the default WAVEFORMATEXTENSIBLE speaker mask)
        // gives the true total channel count and speaker mask for the whole file. A
        // plain mono/stereo file (a single stream) carries none of these and falls back
        // to this first (and only) block's own header flags, as before.
        let channel_info = if have_sub_buf { find_channel_info(&sub_buf) } else { None };
        let channel_layout = match channel_info {
            Some((n, mask)) if n > 0 => {
                let positioned = Position::from_bits_truncate(mask as u64);
                if positioned.bits().count_ones() as u16 == n {
                    Channels::Positioned(positioned)
                }
                else {
                    // Mask doesn't match the channel count (malformed, or a file with
                    // "unassigned" channels) -- fall back to discrete channels rather
                    // than risk a plane-count mismatch during decode.
                    Channels::Discrete(n)
                }
            }
            _ => {
                if header.is_stereo() {
                    layouts::CHANNEL_LAYOUT_STEREO
                }
                else {
                    layouts::CHANNEL_LAYOUT_MONO
                }
            }
        };

        let mut codec_params = AudioCodecParameters::new();
        codec_params
            .for_codec(CODEC_ID_WAVPACK)
            .with_bits_per_coded_sample(header.get_bytes_per_sample() * 8)
            .with_bits_per_sample(header.get_bytes_per_sample() * 8)
            .with_channels(channel_layout);

        let sample_format = match header.get_encoding() {
            Encoding::Pcm if header.is_float() => SampleFormat::F32,
            Encoding::Pcm => match header.get_bytes_per_sample() {
                1 => SampleFormat::S8,
                2 => SampleFormat::S16,
                3 => SampleFormat::S24,
                4 => SampleFormat::S32,
                _ => return decode_error("WavPack: Invalid sample format"),
            },
            Encoding::Dsd => return unsupported_error("WavPack: DSD unsupported"),
        };
        codec_params.with_sample_format(sample_format);

        if let Some(sr) = header.get_sample_rate() {
            codec_params.with_sample_rate(sr);
        }

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        // The v4/v5 block header carries the exact total sample count for the whole file
        // (only meaningful in the first block); a sentinel of u32::MAX means unknown.
        if let Some(total_samples) = header.get_total_samples() {
            track.with_num_frames(total_samples);
            track.with_duration(Duration::new(total_samples));
        }

        // Scan block 0's sub-blocks for a RiffHeader sub-block. GrandOrgue / Hauptwerk
        // store loop points (`smpl`) and the release marker (`cue `) inside the WAV
        // chunks that WavPack preserves there. Routing them through the same parsers
        // used by the v3 path keeps the tag layout identical between the two flavors.
        let mut meta_builder = MetadataBuilder::new(MetadataInfo {
            metadata: well_known::METADATA_ID_WAVE,
            short_name: "riff",
            long_name: "RIFF/WAVE Sampler Metadata",
        });
        if have_sub_buf {
            scan_v4v5_riff_meta(&sub_buf, &mut meta_builder)?;
        }

        let revision = meta_builder.build();
        if !revision.media.tags.is_empty() {
            metadata.push(revision);
        }

        mss.seek(std::io::SeekFrom::Start(original_pos))?;

        let media_info = MediaInfo::from_track(&track);

        Ok(WavPackReader {
            reader: mss,
            media_info,
            tracks: vec![track],
            metadata,
            chapters: None,
            next_packet_ts: 0,
            restart_pos: header_pos,
            index: SeekIndex::new(),
            format_version: FormatVersion::V4V5,
            correction: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Probe / FormatReader impl
// ---------------------------------------------------------------------------

impl ProbeableFormat<'_> for WavPackReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(WavPackReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            support_format!(WAVPACK_FORMAT_INFO, &["wv"], &["audio/x-wavpack"], &[b"wvpk"]),
            support_format!(WAVPACK_FORMAT_INFO, &["wv"], &["audio/x-wavpack"], &[b"RIFF"]),
        ]
    }
}

impl Scoreable for WavPackReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl FormatReader for WavPackReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &WAVPACK_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.tracks.is_empty() {
            return decode_error("wavpack: no tracks");
        }
        let ts = self.next_packet_ts;
        let wanted = self.index.wants(ts);
        let entry = IndexEntry {
            ts,
            pos: self.reader.pos(),
            wvc_pos: if wanted {
                self.correction.as_ref().map_or(0, |c| c.resume_pos())
            }
            else {
                0
            },
        };
        let pkt = match &self.format_version {
            FormatVersion::V3 { num_channels, bytes_per_sample } => {
                let (ch, bps) = (*num_channels, *bytes_per_sample);
                self.next_packet_v3(ch, bps)
            }
            FormatVersion::V4V5 => self.next_packet_v4v5(),
        }?;
        if wanted && pkt.is_some() {
            self.index.insert(entry);
        }
        Ok(pkt)
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }
        let time_base = self.tracks[0].time_base;
        let num_frames = self.tracks[0].num_frames;

        let ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb = time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        if let Some(num_frames) = num_frames {
            if ts.get() as u64 > num_frames {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }
        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }
        if self.correction.as_ref().is_some_and(|c| !c.reader.is_seekable()) {
            return seek_error(SeekErrorKind::Unseekable);
        }

        // Start from the closest indexed packet at or before the target (or the first block),
        // and scan forward, rewinding to the start of the block that contains the desired
        // timestamp. The scan extends the index.
        match self.index.find(ts.get()) {
            Some(e) => {
                self.reader.seek(SeekFrom::Start(e.pos))?;
                if let Some(correction) = self.correction.as_mut() {
                    correction.restart_at(e.wvc_pos)?;
                }
                self.next_packet_ts = e.ts;
            }
            None => {
                self.reader.seek(SeekFrom::Start(self.restart_pos))?;
                if let Some(correction) = self.correction.as_mut() {
                    correction.restart()?;
                }
                self.next_packet_ts = 0;
            }
        }

        loop {
            let block_start = self.reader.pos();
            let correction_mark = self.correction.as_ref().map(|c| c.mark());
            let saved_next_ts = self.next_packet_ts;
            let pkt = match self.next_packet()? {
                Some(p) => p,
                None => return seek_error(SeekErrorKind::OutOfRange),
            };
            let next_ts = pkt.pts.saturating_add(pkt.dur);
            if ts < next_ts {
                self.reader.seek(SeekFrom::Start(block_start))?;
                if let (Some(correction), Some(mark)) = (self.correction.as_mut(), correction_mark) {
                    correction.restore(mark)?;
                }
                self.next_packet_ts = saved_next_ts;
                return Ok(SeekedTo { track_id: 0, actual_ts: pkt.pts, required_ts: ts });
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

// ---------------------------------------------------------------------------
// Packet helpers
// ---------------------------------------------------------------------------

impl WavPackReader<'_> {
    fn next_packet_v3(
        &mut self,
        num_channels: u16,
        bytes_per_sample: u16,
    ) -> Result<Option<Packet>> {
        if find_next_block(&mut self.reader, 65536).is_err() {
            return Ok(None);
        }
        let hdr = Header3::decode(&mut self.reader)?;

        let header_payload = Header3::header_payload_size(hdr.version);
        let ck_size = hdr.ck_size as u32;

        // Build packet: 32-byte prefix || compressed audio
        let prefix = hdr.to_prefix(num_channels, bytes_per_sample);
        let mut pkt_data = prefix.to_vec();

        if ck_size > header_payload {
            // Audio bytes are encoded within the block (ck_size includes them). `ck_size` is
            // untrusted, so read incrementally instead of pre-allocating it.
            let audio_size = (ck_size - header_payload) as usize;
            let audio = self.reader.read_boxed_slice_exact(audio_size)?;
            pkt_data.extend_from_slice(&audio);
        } else {
            // Real WavPack 3.97 files: ck_size == header only; compressed audio
            // follows the block header and extends to the next "wvpk" or EOF.
            let audio = read_v3_audio(&mut self.reader)?;
            pkt_data.extend_from_slice(&audio);
        }

        let n_samples = hdr.total_samples.max(0) as u64;
        let ts = self.next_packet_ts;
        self.next_packet_ts += n_samples as i64;

        Ok(Some(Packet::new(
            0,
            Timestamp::new(ts),
            Duration::new(n_samples),
            pkt_data,
        )))
    }

    fn next_packet_v4v5(&mut self) -> Result<Option<Packet>> {
        // A "block group" is one or more consecutive wvpk blocks sharing the same
        // starting sample index: one block per audio stream (a mono or stereo pair),
        // covering all of the file's channels between them (INITIAL_BLOCK on the first,
        // FINAL_BLOCK on the last). For an ordinary mono/stereo file (a single stream)
        // every block has both flags set, so this loop runs exactly once per packet,
        // identical to the previous single-block behaviour.
        let mut streams: Vec<Vec<u8>> = Vec::new();
        let mut block_samples: u32 = 0;

        loop {
            let (header, mini) = match read_v4v5_stream_block(&mut self.reader, self.correction.as_mut())? {
                Some(v) => v,
                None => break,
            };
            if streams.is_empty() {
                block_samples = header.block_samples;
            }
            let is_final = header.is_final_block();
            streams.push(mini);
            if is_final || streams.len() >= MAX_STREAMS_PER_PACKET {
                break;
            }
        }

        if streams.is_empty() {
            return Ok(None);
        }

        let pkt = assemble_packet(&streams);

        // block_samples is the per-channel frame count, shared by every stream in the
        // group (they all cover the same span of the timeline).
        let n  = block_samples as i64;
        let ts = self.next_packet_ts;
        self.next_packet_ts += n;

        Ok(Some(Packet::new(0, Timestamp::new(ts), Duration::new(n as u64), pkt)))
    }
}

/// Maximum number of per-stream blocks merged into a single packet (one multichannel
/// "block group"). A generous safety cap (well above any real WavPack file, which tops
/// out at 256 channels / ~128 stereo-pair streams in practice) so a stream that never
/// sets `FINAL_BLOCK` -- malformed input -- can't grow a packet unboundedly.
const MAX_STREAMS_PER_PACKET: usize = 512;

/// Read one physical wvpk block (one stream of a possibly-multichannel "block group")
/// and serialise it into the per-stream mini-packet the decoder expects:
///
/// ```text
/// flags(4) + block_samples(4) + crc(4)
/// + terms_len(4) + weights_len(4) + samples_len(4) + entropy_len(4)
/// + hybrid_profile_len(4) + float_info_len(4) + int32_len(4) + wvx_len(4)
/// + shaping_len(4) + wvc_len(4) + wvc_wvx_len(4) + wvc_crc(4) + ext_flags(4)
/// ```
///
/// followed by the raw sub-block bytes (in that order) then the audio bitstream. The last five
/// header fields describe the matching block of the `.wvc` correction file, if `correction` is
/// given and has one (see `serialise_stream_packet`).
///
/// Returns `Ok(None)` at a clean end of stream (no more `wvpk` markers found).
fn read_v4v5_stream_block(
    reader: &mut MediaSourceStream<'_>,
    correction: Option<&mut Correction<'_>>,
) -> Result<Option<(Header, Vec<u8>)>> {
    if find_next_block(reader, 10000).is_err() {
        return Ok(None);
    }
    let header = Header::decode(reader)?;

    // ck_size counts everything after the ck_size field itself; the remaining 24 bytes
    // of the 32-byte header have already been consumed by `Header::decode`. Bounding the
    // sub-block loop by this (rather than stopping at the first `WvBitStream`) is
    // required to also pick up sub-blocks that follow the audio bitstream, such as
    // `ID_WVX_BITSTREAM` (float/int32 extension bits) or a trailing `ID_BLOCK_CHECKSUM`.
    let sub_blocks_len = (header.ck_size as u64).saturating_sub(24);
    let end_pos = reader.pos().saturating_add(sub_blocks_len);

    let parts = read_block_parts(reader, end_pos)?;

    // Blocks without samples (e.g. a trailing wrapper or tag block) have no correction block.
    let wvc = match correction {
        Some(c) if header.block_samples != 0 => c.block_for(&header),
        _ => None,
    };

    let mini = serialise_stream_packet(
        header.flags,
        header.block_samples,
        header.crc,
        &parts,
        wvc.as_ref(),
    );

    Ok(Some((header, mini)))
}

// ---------------------------------------------------------------------------
// `.wvc` correction stream
// ---------------------------------------------------------------------------

/// One block of a `.wvc` correction file.
#[derive(Clone)]
struct WvcBlock {
    block_index:   u64,
    block_samples: u32,
    flags:         u32,
    /// The CRC of the *lossless* samples of the block.
    crc:           u32,
    parts:         BlockParts,
}

/// The `.wvc` correction stream that accompanies the main (`.wv`) stream.
struct Correction<'a> {
    reader: MediaSourceStream<'a>,
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
struct CorrectionMark {
    pos: u64,
    pending: Option<WvcBlock>,
    pending_pos: u64,
    done: bool,
}

impl<'a> Correction<'a> {
    /// Locate the first block of the correction stream.
    fn new(mut reader: MediaSourceStream<'a>) -> Result<Self> {
        find_next_block(&mut reader, 10000)?;
        let restart_pos = reader.pos();
        Ok(Correction { reader, restart_pos, pending: None, pending_pos: 0, done: false })
    }

    fn mark(&self) -> CorrectionMark {
        CorrectionMark {
            pos: self.reader.pos(),
            pending: self.pending.clone(),
            pending_pos: self.pending_pos,
            done: self.done,
        }
    }

    fn restore(&mut self, mark: CorrectionMark) -> Result<()> {
        self.reader.seek(SeekFrom::Start(mark.pos))?;
        self.pending = mark.pending;
        self.pending_pos = mark.pending_pos;
        self.done = mark.done;
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        self.restart_at(self.restart_pos)
    }

    /// Resume reading at `pos`, a position returned by `resume_pos`.
    fn restart_at(&mut self, pos: u64) -> Result<()> {
        self.reader.seek(SeekFrom::Start(pos))?;
        self.pending = None;
        self.done = false;
        Ok(())
    }

    /// The position from which reading resumes with the same blocks left to match: the start of
    /// the block read ahead (if any), otherwise the current position.
    fn resume_pos(&self) -> u64 {
        if self.pending.is_some() { self.pending_pos } else { self.reader.pos() }
    }

    /// Read the next block of the correction stream.
    fn read_block(&mut self) -> Option<WvcBlock> {
        find_next_block(&mut self.reader, 10000).ok()?;
        let header = Header::decode(&mut self.reader).ok()?;

        let sub_blocks_len = (header.ck_size as u64).saturating_sub(24);
        let end_pos = self.reader.pos().saturating_add(sub_blocks_len);
        let parts = read_block_parts(&mut self.reader, end_pos).ok()?;

        Some(WvcBlock {
            block_index: header.get_block_index(),
            block_samples: header.block_samples,
            flags: header.flags,
            crc: header.crc,
            parts,
        })
    }

    /// Find the correction block that matches the main block with header `wv`, reading ahead
    /// (and discarding stale blocks) as needed. Port of `read_wvc_block()` in open_utils.c.
    /// `None` means that there is no matching block, and the block is decoded lossy.
    fn block_for(&mut self, wv: &Header) -> Option<WvcBlock> {
        loop {
            if self.pending.is_none() {
                let start = self.reader.pos();
                if self.done {
                    return None;
                }
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
            if block.block_samples == 0 {
                self.pending = None;
                continue;
            }

            match match_wvc_header(wv, block) {
                // A match.
                0 => return self.pending.take(),
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
fn match_wvc_header(wv: &Header, wvc: &WvcBlock) -> i32 {
    let wv_index = wv.get_block_index();

    if wv_index == wvc.block_index && wv.block_samples == wvc.block_samples {
        if wv.flags == wvc.flags {
            return 0;
        }

        let position = |flags: u32| -> i32 {
            let mut p = 0;
            if flags & FLAG_INITIAL_BLOCK != 0 {
                p -= 1;
            }
            if flags & FLAG_FINAL_BLOCK != 0 {
                p += 1;
            }
            p
        };

        return if position(wvc.flags) - position(wv.flags) < 0 { 1 } else { -1 };
    }

    // Block indices are 40-bit: a negative 40-bit difference means the correction block is
    // the earlier one.
    if wvc.block_index.wrapping_sub(wv_index) & 0x80_0000_0000 != 0 { 1 } else { -1 }
}

/// The sub-blocks of one WavPack block that the decoder needs.
#[derive(Default, Clone)]
struct BlockParts {
    terms:   Vec<u8>,
    weights: Vec<u8>,
    samples: Vec<u8>,
    entropy: Vec<u8>,
    hybrid:  Vec<u8>,
    float:   Vec<u8>,
    int32:   Vec<u8>,
    /// `ID_WVX_BITSTREAM` or `ID_WVX_NEW_BITSTREAM` (see `wvx_new`).
    wvx:     Vec<u8>,
    wvx_new: bool,
    shaping: Vec<u8>,
    /// `ID_WVC_BITSTREAM`: only in blocks of a `.wvc` file.
    wvc:     Vec<u8>,
    audio:   Vec<u8>,
}

/// Read sub-blocks from `reader` until its position reaches `end_pos`.
fn read_block_parts<R: ReadBytes>(reader: &mut R, end_pos: u64) -> Result<BlockParts> {
    let mut parts = BlockParts::default();

    while reader.pos() < end_pos {
        let sb = decode_sub_block(reader)?;
        match sb {
            SubBlock::DecorrelationTerms(d)   => parts.terms   = d,
            SubBlock::DecorrelationWeights(d) => parts.weights = d,
            SubBlock::DecorrelationSamples(d) => parts.samples = d,
            SubBlock::EntropyVariables(d)     => parts.entropy = d,
            SubBlock::HybridProfile(d)        => parts.hybrid  = d,
            SubBlock::FloatInfo(d)            => parts.float   = d,
            SubBlock::Int32Info(d)            => parts.int32   = d,
            SubBlock::ShapingWeights(d)       => parts.shaping = d,
            SubBlock::WvBitStream(d)          => parts.audio   = d,
            SubBlock::WvcBitStream(d)         => parts.wvc     = d,
            SubBlock::WvxBitStream(d) => {
                parts.wvx = d;
                parts.wvx_new = false;
            }
            SubBlock::WvxNewBitStream(d) => {
                parts.wvx = d;
                parts.wvx_new = true;
            }
            SubBlock::DsdBlock(_) => {
                return symphonia_core::errors::unsupported_error("wavpack: DSD not supported");
            }
            // Skip everything else: ChannelInfo (already scanned separately, once, in
            // `try_new_v4v5`), metadata, checksums, RIFF headers.
            _ => debug!("v4v5: skipping non-audio sub-block"),
        }
    }

    Ok(parts)
}

/// Read sub-blocks from `reader` until its position reaches `end_pos` and serialise them (see
/// `read_v4v5_stream_block` for the layout) into the per-stream mini-packet the decoder expects.
fn build_stream_packet<R: ReadBytes>(
    reader: &mut R,
    flags: u32,
    block_samples: u32,
    crc: u32,
    end_pos: u64,
) -> Result<Vec<u8>> {
    let parts = read_block_parts(reader, end_pos)?;
    Ok(serialise_stream_packet(flags, block_samples, crc, &parts, None))
}

/// Serialise the parts of a block, and of its matching `.wvc` block if there is one, into a
/// per-stream mini-packet (see `read_v4v5_stream_block` for the layout).
fn serialise_stream_packet(
    flags: u32,
    block_samples: u32,
    crc: u32,
    parts: &BlockParts,
    wvc: Option<&WvcBlock>,
) -> Vec<u8> {
    // A correction block without a correction bitstream is of no use.
    let wvc = wvc.filter(|w| !w.parts.wvc.is_empty());
    let empty: &[u8] = &[];
    let (shaping, wvc_data, wvc_wvx) = match wvc {
        Some(w) => (w.parts.shaping.as_slice(), w.parts.wvc.as_slice(), w.parts.wvx.as_slice()),
        None => (empty, empty, empty),
    };
    let wvc_crc = wvc.map_or(0, |w| w.crc);

    let mut ext_flags = 0u32;
    if parts.wvx_new {
        ext_flags |= EXT_WVX_NEW;
    }
    if wvc.is_some_and(|w| w.parts.wvx_new) {
        ext_flags |= EXT_WVC_WVX_NEW;
    }

    let sections: [&[u8]; 11] = [
        &parts.terms,
        &parts.weights,
        &parts.samples,
        &parts.entropy,
        &parts.hybrid,
        &parts.float,
        &parts.int32,
        &parts.wvx,
        shaping,
        wvc_data,
        wvc_wvx,
    ];

    let payload_len: usize = sections.iter().map(|s| s.len()).sum::<usize>() + parts.audio.len();
    let mut mini: Vec<u8> = Vec::with_capacity(STREAM_HDR + payload_len);
    mini.extend_from_slice(&flags.to_le_bytes());
    mini.extend_from_slice(&block_samples.to_le_bytes());
    mini.extend_from_slice(&crc.to_le_bytes());
    for s in &sections {
        mini.extend_from_slice(&(s.len() as u32).to_le_bytes());
    }
    mini.extend_from_slice(&wvc_crc.to_le_bytes());
    mini.extend_from_slice(&ext_flags.to_le_bytes());
    debug_assert_eq!(mini.len(), STREAM_HDR);

    for s in &sections {
        mini.extend_from_slice(s);
    }
    mini.extend_from_slice(&parts.audio);

    mini
}

/// `INITIAL_BLOCK` (wavpack.h `0x800`).
const FLAG_INITIAL_BLOCK: u32 = 0x0000_0800;
/// `FINAL_BLOCK` (wavpack.h `0x1000`).
const FLAG_FINAL_BLOCK: u32 = 0x0000_1000;

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
pub(crate) fn matroska_block_to_packet(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(data);

    let block_samples = reader.read_u32()?;

    let mut streams: Vec<Vec<u8>> = Vec::new();

    while (reader.pos() as usize) < data.len() && streams.len() < MAX_STREAMS_PER_PACKET {
        let flags = reader.read_u32()?;
        let crc = reader.read_u32()?;

        let remaining = data.len() - reader.pos() as usize;
        let single_block = flags & (FLAG_INITIAL_BLOCK | FLAG_FINAL_BLOCK)
            == (FLAG_INITIAL_BLOCK | FLAG_FINAL_BLOCK);

        let size = if single_block { remaining } else { reader.read_u32()? as usize };

        let start = reader.pos() as usize;
        if size > data.len() - start {
            return decode_error("wavpack: matroska block size exceeds frame");
        }
        let mut payload = BufReader::new(&data[start..start + size]);
        streams.push(build_stream_packet(&mut payload, flags, block_samples, crc, size as u64)?);
        reader.ignore_bytes(size as u64)?;

        if flags & FLAG_FINAL_BLOCK != 0 {
            break;
        }
    }

    if streams.is_empty() {
        return decode_error("wavpack: empty matroska block");
    }

    Ok(assemble_packet(&streams))
}

/// Serialise per-stream mini-packets into a `WV45` packet.
///
/// Each stream's mini-block is prefixed with its own byte length so the decoder
/// can slice out exactly its bytes (needed because the mini-block's audio
/// bitstream is otherwise unbounded -- it runs to "the end of this stream's
/// data", which is only unambiguous once we know where this stream ends and
/// the next one's header begins).
fn assemble_packet(streams: &[Vec<u8>]) -> Vec<u8> {
    let payload_len: usize = streams.iter().map(|s| 4 + s.len()).sum();
    let mut pkt: Vec<u8> = Vec::with_capacity(8 + payload_len);
    pkt.extend_from_slice(b"WV45");
    pkt.extend_from_slice(&(streams.len() as u32).to_le_bytes());
    for s in streams {
        pkt.extend_from_slice(&(s.len() as u32).to_le_bytes());
        pkt.extend_from_slice(s);
    }
    pkt
}

// ---------------------------------------------------------------------------
// RIFF mark parsers
// ---------------------------------------------------------------------------

/// Parse a `smpl` chunk and add its fields as `WavPack/*` tags.
///
/// Tag keys follow the `WavPack/<Field>` convention so applications can
/// retrieve them without depending on format-specific types. The GrandOrgue
/// sampler chunk layout (`GO_WAVESAMPLERCHUNK` / `GO_WAVESAMPLERLOOP`) is
/// used as the authoritative field mapping.
fn parse_smpl_chunk(
    source: &mut MediaSourceStream<'_>,
    chunk_size: u32,
    builder: &mut MetadataBuilder,
) -> Result<()> {
    // GO_WAVESAMPLERCHUNK: 9 × u32 = 36 bytes
    const SAMPLER_HDR: u32 = 36;
    // GO_WAVESAMPLERLOOP: 6 × u32 = 24 bytes
    const LOOP_ENTRY:  u32 = 24;

    if chunk_size < SAMPLER_HDR {
        debug!("wavpack: smpl chunk too small ({})", chunk_size);
        mss_skip(source, chunk_size as u64)?;
        return Ok(());
    }

    let _manufacturer  = source.read_u32()?;
    let _product       = source.read_u32()?;
    let sample_period  = source.read_u32()?;
    let midi_note      = source.read_u32()?;
    let pitch_fraction = source.read_u32()?;
    let _smpte_format  = source.read_u32()?;
    let _smpte_offset  = source.read_u32()?;
    let num_loops      = source.read_u32()?;
    let _sampler_data  = source.read_u32()?;

    builder.add_tag(Tag::new_from_parts("WavPack/MidiNote",      midi_note,      None));
    builder.add_tag(Tag::new_from_parts("WavPack/PitchFraction", pitch_fraction, None));
    // Sample period (ns/sample) is used by Hauptwerk for fine tuning when it
    // disagrees with the format chunk's sample rate. Skip the trivial case
    // (0 = "unspecified") so the tag set stays meaningful.
    if sample_period != 0 {
        builder.add_tag(Tag::new_from_parts("WavPack/SamplePeriod", sample_period, None));
    }
    builder.add_tag(Tag::new_from_parts("WavPack/LoopCount", num_loops, None));

    let loops_size = num_loops.saturating_mul(LOOP_ENTRY);
    let remaining  = chunk_size.saturating_sub(SAMPLER_HDR);

    if loops_size > remaining {
        debug!("wavpack: smpl loop count exceeds chunk size");
        mss_skip(source, remaining as u64)?;
        return Ok(());
    }

    for i in 0..num_loops {
        let _id        = source.read_u32()?;
        let loop_type  = source.read_u32()?;
        let start      = source.read_u32()?;
        let end        = source.read_u32()?;
        let fraction   = source.read_u32()?;
        let play_cnt   = source.read_u32()?;
        // Hauptwerk uses Type (0=forward, 1=alternating, 2=reverse) and
        // PlayCount (0 = infinite). GrandOrgue only consults Start/End but
        // is fine with the extras being present.
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/Type"),      loop_type, None));
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/Start"),     start,     None));
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/End"),       end,       None));
        if fraction != 0 {
            builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/Fraction"),  fraction, None));
        }
        if play_cnt != 0 {
            builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/PlayCount"), play_cnt, None));
        }
    }

    let consumed = SAMPLER_HDR + loops_size;
    if chunk_size > consumed {
        mss_skip(source, (chunk_size - consumed) as u64)?;
    }
    Ok(())
}

/// Parse a `cue ` chunk and store the release point as a tag.
///
/// GrandOrgue identifies the release point as the highest `dwSampleOffset`
/// across all cue points (`GO_WAVECUEPOINT`). That sample offset is stored
/// under `"WavPack/ReleasePoint"`.
fn parse_cue_chunk(
    source: &mut MediaSourceStream<'_>,
    chunk_size: u32,
    builder: &mut MetadataBuilder,
) -> Result<()> {
    // GO_WAVECUECHUNK:  1 × u32 = 4 bytes
    // GO_WAVECUEPOINT:  6 × u32 = 24 bytes
    //   (dwName, dwPosition, fccChunk, dwChunkStart, dwBlockStart, dwSampleOffset)
    const CUE_HDR:   u32 = 4;
    const CUE_ENTRY: u32 = 24;

    if chunk_size < CUE_HDR {
        debug!("wavpack: cue chunk too small");
        mss_skip(source, chunk_size as u64)?;
        return Ok(());
    }

    let num_cues   = source.read_u32()?;
    let entries_sz = num_cues.saturating_mul(CUE_ENTRY);
    let remaining  = chunk_size.saturating_sub(CUE_HDR);

    if entries_sz > remaining {
        debug!("wavpack: cue count exceeds chunk size");
        mss_skip(source, remaining as u64)?;
        return Ok(());
    }

    builder.add_tag(Tag::new_from_parts("WavPack/CueCount", num_cues, None));

    // Keep two views: each cue point individually (Hauptwerk's
    // multi-release-stage convention) and the highest sample offset as
    // "ReleasePoint" (GrandOrgue's single-marker convention).
    let mut release_point: Option<u32> = None;
    for i in 0..num_cues {
        let name          = source.read_u32()?;
        let _position     = source.read_u32()?;
        let _fcc_chunk    = source.read_u32()?;
        let _chunk_start  = source.read_u32()?;
        let _block_start  = source.read_u32()?;
        let sample_offset = source.read_u32()?;

        builder.add_tag(Tag::new_from_parts(format!("WavPack/Cue{i}/Name"),         name,          None));
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Cue{i}/SampleOffset"), sample_offset, None));

        release_point = Some(match release_point {
            Some(prev) => prev.max(sample_offset),
            None       => sample_offset,
        });
    }

    if let Some(rp) = release_point {
        builder.add_tag(Tag::new_from_parts("WavPack/ReleasePoint", rp, None));
    }

    let consumed = CUE_HDR + entries_sz;
    if chunk_size > consumed {
        mss_skip(source, (chunk_size - consumed) as u64)?;
    }
    Ok(())
}

#[inline]
fn mss_skip(source: &mut MediaSourceStream<'_>, n: u64) -> Result<()> {
    source.ignore_bytes(n).map_err(symphonia_core::errors::Error::IoError)
}

// ---------------------------------------------------------------------------
// V4/V5 RIFF metadata scan
// ---------------------------------------------------------------------------

// Sub-block IDs we care about for metadata extraction.
const SBID_RIFF_HEADER:  u8 = 0x21;
const SBID_RIFF_TRAILER: u8 = 0x22;

/// `ID_CHANNEL_INFO` (wavpack.h `0x0D`): total channel count + WAVEFORMATEXTENSIBLE-style
/// speaker mask for a multichannel file, present once (in the first block) whenever a
/// file has more than 2 channels or channels that don't map to the plain mono/stereo
/// default. Port of `read_channel_info()` in open_utils.c (both the legacy 1-5 byte and
/// the "new" (WavPack 5.0+, `>= 6` byte) unlimited-channel-count encodings).
fn find_channel_info(body: &[u8]) -> Option<(u16, u32)> {
    let mut p = 0usize;
    while p + 2 <= body.len() {
        let id = body[p];
        let (hdr_len, words) = if id & 0x80 != 0 {
            if p + 4 > body.len() { break; }
            let w = (body[p + 1] as u32) | ((body[p + 2] as u32) << 8) | ((body[p + 3] as u32) << 16);
            (4usize, w)
        }
        else {
            (2usize, body[p + 1] as u32)
        };

        let data_len = (words as usize) * 2;
        let pad      = if id & 0x40 != 0 { 1 } else { 0 };
        let start    = p + hdr_len;
        let end_full = start + data_len;
        if end_full > body.len() { break; }
        let end_used = end_full - pad;

        if id & 0x3F == 0x0D {
            let data = &body[start..end_used];
            let bytecnt = data.len();
            if bytecnt == 0 || bytecnt > 7 {
                return None;
            }

            if bytecnt >= 6 {
                // "New" (2016+) format: 3-byte channel-count/max-streams pair (each
                // extended with the low/high nibble of the 3rd byte for >255 channels)
                // followed by a 3 or 4-byte little-endian channel mask.
                let num_channels = (data[0] as u32 | (((data[2] & 0xf) as u32) << 8)) + 1;
                let mut mask = data[3] as u32 | ((data[4] as u32) << 8) | ((data[5] as u32) << 16);
                if bytecnt == 7 {
                    mask |= (data[6] as u32) << 24;
                }
                return Some((num_channels.min(u16::MAX as u32) as u16, mask));
            }
            else {
                // Legacy format: 1-byte channel count followed by up to 4 mask bytes.
                let num_channels = data[0] as u32;
                let mut mask = 0u32;
                let mut shift = 0u32;
                for &b in &data[1..] {
                    mask |= (b as u32) << shift;
                    shift += 8;
                }
                return Some((num_channels.min(u16::MAX as u32) as u16, mask));
            }
        }

        p = end_full;
    }
    None
}

/// Walk the sub-blocks contained in the first wvpk block body, locate any
/// RIFF header bytes WavPack stashed there (sub-block IDs 0x21 / 0x22), and
/// parse `smpl` and `cue ` chunks out of them into builder tags.
fn scan_v4v5_riff_meta(body: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    let mut p = 0usize;
    while p + 2 <= body.len() {
        let id = body[p];
        // ID_LARGE_BLOCK uses a 3-byte size field, otherwise 1-byte.
        let (hdr_len, words) = if id & 0x80 != 0 {
            if p + 4 > body.len() { break; }
            let w = (body[p + 1] as u32)
                  | ((body[p + 2] as u32) << 8)
                  | ((body[p + 3] as u32) << 16);
            (4usize, w)
        } else {
            (2usize, body[p + 1] as u32)
        };

        let data_len = (words as usize) * 2;
        let pad      = if id & 0x40 != 0 { 1 } else { 0 };
        let start    = p + hdr_len;
        let end_full = start + data_len;
        if end_full > body.len() { break; }
        let end_used = end_full - pad;

        let fn_id = id & 0x3F;
        if fn_id == SBID_RIFF_HEADER || fn_id == SBID_RIFF_TRAILER {
            parse_riff_meta_bytes(&body[start..end_used], builder)?;
        }

        p = end_full;
    }
    Ok(())
}

/// Parse RIFF chunks out of a raw byte buffer (with or without a leading
/// `RIFF....WAVE` envelope) and feed `smpl` / `cue ` into the existing
/// chunk parsers via a Cursor-backed MediaSourceStream.
fn parse_riff_meta_bytes(buf: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    use symphonia_core::io::MediaSourceStreamOptions;

    let mut start = 0usize;
    if buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
        start = 12;
    } else if buf.len() >= 4 && &buf[0..4] == b"WAVE" {
        start = 4;
    }

    let chunks = buf[start..].to_vec();
    let cursor = std::io::Cursor::new(chunks);
    let mut mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());

    while let Ok(chunk_id) = mss.read_quad_bytes() {
        let chunk_size = match mss.read_u32() {
            Ok(v) => v,
            Err(_) => break,
        };

        match &chunk_id {
            b"smpl" => {
                parse_smpl_chunk(&mut mss, chunk_size, builder)?;
                if chunk_size % 2 != 0 { let _ = mss.read_u8(); }
            }
            b"cue " => {
                parse_cue_chunk(&mut mss, chunk_size, builder)?;
                if chunk_size % 2 != 0 { let _ = mss.read_u8(); }
            }
            b"data" => {
                // We don't expect 'data' inside a RiffHeader sub-block payload
                // (audio is in the WvBitStream sub-block), but if encountered
                // the bytes after it would be PCM and we should stop walking
                // metadata.
                break;
            }
            _ => {
                let skip = (chunk_size as u64) + (chunk_size as u64 % 2);
                if mss.ignore_bytes(skip).is_err() { break; }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// V3 structures
// ---------------------------------------------------------------------------

struct WaveHeader3 {
    sample_rate:     u32,
    num_channels:    u16,
    bits_per_sample: u16,
    bytes_per_sample: u16,
}

struct Header3 {
    ck_size:       i32,
    version:       i16,
    bits:          i16,
    flags:         i16,
    shift:         i16,
    total_samples: i32,
    crc:           i32,
    crc2:          i32,
    ext:           [u8; 4],
    extra_bc:      u8,
    extras:        [u8; 3],
}

impl Header3 {
    // Payload bytes consumed after ck_size, per version.
    fn header_payload_size(version: i16) -> u32 {
        match version {
            1 => 2,
            2 => 4,
            _ => 28,
        }
    }

    fn decode(reader: &mut MediaSourceStream<'_>) -> Result<Header3> {
        let marker = reader.read_quad_bytes()?;
        if marker != STREAM_MARKER {
            return decode_error("wavpack v3: missing wvpk marker");
        }
        let ck_size = reader.read_i32()?;
        let version = reader.read_i16()?;
        if version < 1 || version > 3 {
            return decode_error("wavpack: unsupported v3 block version");
        }

        let bits = if version >= 2 { reader.read_i16()? } else { 0 };

        let (flags, shift, total_samples, crc, crc2, ext, extra_bc, extras) = if version >= 3 {
            let flags         = reader.read_i16()?;
            let shift         = reader.read_i16()?;
            let total_samples = reader.read_i32()?;
            let crc           = reader.read_i32()?;
            let crc2          = reader.read_i32()?;
            let mut ext = [0u8; 4];
            reader.read_buf_exact(&mut ext)?;
            let extra_bc      = reader.read_u8()?;
            let mut extras = [0u8; 3];
            reader.read_buf_exact(&mut extras)?;
            (flags, shift, total_samples, crc, crc2, ext, extra_bc, extras)
        } else {
            (0, 0, 0, 0, 0, [0u8; 4], 0u8, [0u8; 3])
        };

        Ok(Header3 { ck_size, version, bits, flags, shift, total_samples, crc, crc2, ext, extra_bc, extras })
    }

    /// Serialise the header into a 32-byte packet prefix understood by `WavPackDecoder`.
    fn to_prefix(&self, num_channels: u16, bytes_per_sample: u16) -> [u8; 32] {
        let mut p = [0u8; 32];
        p[0..2].copy_from_slice(&self.version.to_le_bytes());
        p[2..4].copy_from_slice(&self.bits.to_le_bytes());
        p[4..6].copy_from_slice(&self.flags.to_le_bytes());
        p[6..8].copy_from_slice(&self.shift.to_le_bytes());
        p[8..12].copy_from_slice(&self.total_samples.to_le_bytes());
        p[12..16].copy_from_slice(&self.crc.to_le_bytes());
        p[16..20].copy_from_slice(&self.crc2.to_le_bytes());
        p[20..24].copy_from_slice(&self.ext);
        p[24] = self.extra_bc;
        p[25..28].copy_from_slice(&self.extras);
        p[28..30].copy_from_slice(&num_channels.to_le_bytes());
        p[30..32].copy_from_slice(&bytes_per_sample.to_le_bytes());
        p
    }
}

// ---------------------------------------------------------------------------
// V4/V5 structures
// ---------------------------------------------------------------------------

struct Header {
    ck_size:           u32,
    #[allow(dead_code)]
    version:           u16,
    block_index_u8:    u8,
    total_samples_u8:  u8,
    total_samples_u32: u32,
    block_index_u32:   u32,
    block_samples:     u32,
    flags:             u32,
    crc:               u32,
}

impl Header {
    fn decode(reader: &mut MediaSourceStream<'_>) -> Result<Header> {
        let marker = reader.read_quad_bytes()?;
        if marker != STREAM_MARKER {
            return unsupported_error("wavpack: missing marker");
        }
        Ok(Header {
            ck_size:           reader.read_u32()?,
            version:           reader.read_u16()?,
            block_index_u8:    reader.read_u8()?,
            total_samples_u8:  reader.read_u8()?,
            total_samples_u32: reader.read_u32()?,
            block_index_u32:   reader.read_u32()?,
            block_samples:     reader.read_u32()?,
            flags:             reader.read_u32()?,
            crc:               reader.read_u32()?,
        })
    }

    fn get_block_index(&self) -> u64 {
        combine_values!(self.block_index_u32, self.block_index_u8)
    }

    fn get_bytes_per_sample(&self) -> u32 {
        (self.flags & 3) + 1
    }

    fn get_encoding(&self) -> Encoding {
        if (self.flags >> 31) & 1 == 0 { Encoding::Pcm } else { Encoding::Dsd }
    }

    /// `FLOAT_DATA` (wavpack.h `0x80`): the block carries IEEE-754 32-bit float samples
    /// (shifted-mantissa integers here) rather than plain PCM integers.
    fn is_float(&self) -> bool {
        (self.flags & 0x0000_0080) != 0
    }

    /// `FINAL_BLOCK` (wavpack.h `0x1000`): this is the last block of a multichannel
    /// "block group" sharing the same starting sample index. For ordinary mono/stereo
    /// files (a single stream) every block has this flag set, since each block is both
    /// the first and last block of its own (single-stream) group.
    fn is_final_block(&self) -> bool {
        (self.flags & 0x0000_1000) != 0
    }

    fn is_stereo(&self) -> bool {
        ((self.flags >> 2) & 1) == 0
    }

    fn get_sample_rate(&self) -> Option<u32> {
        let idx = (self.flags >> 23) & 0xF;
        if idx == 0xF {
            return None;
        }
        SAMPLE_RATES.get(idx as usize).copied()
    }

    /// The total number of samples (frames) in the whole file, as recorded in the first
    /// block's header. `u32::MAX` in the low 32 bits is the WavPack sentinel for "unknown"
    /// (e.g. streaming encodes that didn't know the final length up front).
    fn get_total_samples(&self) -> Option<u64> {
        if self.total_samples_u32 == u32::MAX {
            None
        } else {
            Some(combine_values!(self.total_samples_u32, self.total_samples_u8))
        }
    }
}

/// Read compressed audio bytes from `reader` until the next `wvpk` marker or EOF.
///
/// Used for real WavPack 3.97 files where `ck_size` only covers the block header
/// metadata and the compressed audio runs implicitly to the next block boundary.
fn read_v3_audio(reader: &mut MediaSourceStream<'_>) -> Result<Vec<u8>> {
    // We need 4 bytes of seekback so we can "unread" the next "wvpk" marker.
    reader.ensure_seekback_buffer(4);

    let mut audio: Vec<u8> = Vec::new();
    loop {
        let b = match reader.read_u8() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(audio),
            Err(e) => return Err(symphonia_core::errors::Error::IoError(e)),
        };
        audio.push(b);
        let n = audio.len();
        if n >= 4 && &audio[n - 4..] == b"wvpk" {
            audio.truncate(n - 4);
            reader.seek_buffered_rev(4);
            return Ok(audio);
        }
    }
}

fn find_next_block(source: &mut MediaSourceStream<'_>, max_bytes: usize) -> Result<u64> {
    let mut n = 0usize;
    source.ensure_seekback_buffer(max_bytes);
    loop {
        if n + 4 >= max_bytes {
            return decode_error("no block found");
        }
        let b = source.read_u8()?;
        n += 1;
        if b == b'w' {
            let t = source.read_triple_bytes()?;
            n += 3;
            if t == *b"vpk" {
                source.seek_buffered_rev(4);
                return Ok(n as u64);
            }
        }
    }
}

/// Pre-scan every WavPack v3 block header starting at `data_start_pos`, summing
/// `total_samples` across the whole file to recover the track's exact frame count.
///
/// The RIFF/WAVE wrapper around a v1-v3 stream has no field for the overall sample
/// count, so it must be derived by walking the block headers (32 bytes each, audio
/// payload skipped without decoding). Requires a seekable source; the stream is left
/// exactly where it started (`data_start_pos`) so subsequent packet reads are unaffected.
fn scan_v3_total_samples(mss: &mut MediaSourceStream<'_>, data_start_pos: u64) -> Option<u64> {
    if !mss.is_seekable() {
        return None;
    }

    let mut total: u64 = 0;
    loop {
        if find_next_block(mss, 65536).is_err() {
            break;
        }
        let hdr = match Header3::decode(mss) {
            Ok(h) => h,
            Err(_) => break,
        };
        total = total.saturating_add(hdr.total_samples.max(0) as u64);

        let header_payload = Header3::header_payload_size(hdr.version);
        let ck_size = hdr.ck_size as u32;
        let skip_ok = if ck_size > header_payload {
            mss.ignore_bytes((ck_size - header_payload) as u64).is_ok()
        } else {
            read_v3_audio(mss).is_ok()
        };
        if !skip_ok {
            break;
        }
    }

    // Restore the stream to where packet reading actually begins.
    let _ = mss.seek(std::io::SeekFrom::Start(data_start_pos));
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL: u32 = FLAG_INITIAL_BLOCK;
    const FINAL: u32 = FLAG_FINAL_BLOCK;

    /// Create a sub-block with the ID `id`, and the data `data` (of an even length).
    fn sub_block(id: u8, data: &[u8]) -> Vec<u8> {
        assert!(data.len() % 2 == 0 && data.len() / 2 < 0x80);
        [&[id, (data.len() / 2) as u8], data].concat()
    }

    /// The lengths of the terms, weights, samples, entropy, hybrid, float, int32, and wvx data
    /// that follow the first 12 bytes in a stream of a packet.
    fn stream_lens(stream: &[u8]) -> Vec<u32> {
        (0..8)
            .map(|i| u32::from_le_bytes(stream[12 + 4 * i..16 + 4 * i].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn verify_matroska_single_block_to_packet() {
        // A stereo block: `block_samples`, `flags`, `crc`, and the sub-blocks.
        let flags = INITIAL | FINAL | 0x1;

        let mut frame = Vec::new();
        frame.extend(4096u32.to_le_bytes());
        frame.extend(flags.to_le_bytes());
        frame.extend(0xdead_beefu32.to_le_bytes());
        frame.extend(sub_block(0x02, &[1, 2, 3, 4]));
        frame.extend(sub_block(0x0a, &[9, 8, 7, 6, 5, 4]));

        let pkt = matroska_block_to_packet(&frame).unwrap();

        // Header: magic, 1 stream, length of the stream.
        assert_eq!(&pkt[0..4], b"WV45");
        assert_eq!(u32::from_le_bytes(pkt[4..8].try_into().unwrap()), 1);
        let len = u32::from_le_bytes(pkt[8..12].try_into().unwrap()) as usize;
        assert_eq!(pkt.len(), 12 + len);

        let stream = &pkt[12..];
        assert_eq!(u32::from_le_bytes(stream[0..4].try_into().unwrap()), flags);
        assert_eq!(u32::from_le_bytes(stream[4..8].try_into().unwrap()), 4096);
        assert_eq!(u32::from_le_bytes(stream[8..12].try_into().unwrap()), 0xdead_beef);
        assert_eq!(stream_lens(stream), [4, 0, 0, 0, 0, 0, 0, 0]);
        // The decorrelation terms, followed by the bitstream.
        assert_eq!(&stream[crate::decoder::STREAM_HDR..], &[1, 2, 3, 4, 9, 8, 7, 6, 5, 4]);
    }

    #[test]
    fn verify_matroska_multichannel_block_to_packet() {
        // Two blocks (two stereo streams). Unlike a single block, the blocks have a size.
        let first = sub_block(0x0a, &[1, 2, 3, 4]);
        let second = sub_block(0x0a, &[5, 6]);

        let mut frame = Vec::new();
        frame.extend(1024u32.to_le_bytes());

        frame.extend(INITIAL.to_le_bytes());
        frame.extend(1u32.to_le_bytes());
        frame.extend((first.len() as u32).to_le_bytes());
        frame.extend(&first);

        frame.extend(FINAL.to_le_bytes());
        frame.extend(2u32.to_le_bytes());
        frame.extend((second.len() as u32).to_le_bytes());
        frame.extend(&second);

        let pkt = matroska_block_to_packet(&frame).unwrap();
        assert_eq!(u32::from_le_bytes(pkt[4..8].try_into().unwrap()), 2);

        let len = u32::from_le_bytes(pkt[8..12].try_into().unwrap()) as usize;
        let stream0 = &pkt[12..12 + len];
        assert_eq!(u32::from_le_bytes(stream0[0..4].try_into().unwrap()), INITIAL);
        assert_eq!(&stream0[crate::decoder::STREAM_HDR..], &[1, 2, 3, 4]);

        // All blocks share the number of samples.
        let rest = &pkt[12 + len..];
        let len = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        let stream1 = &rest[4..4 + len];
        assert_eq!(u32::from_le_bytes(stream1[0..4].try_into().unwrap()), FINAL);
        assert_eq!(u32::from_le_bytes(stream1[4..8].try_into().unwrap()), 1024);
        assert_eq!(&stream1[crate::decoder::STREAM_HDR..], &[5, 6]);
        assert_eq!(rest.len(), 4 + len);
    }

    #[test]
    fn verify_malformed_matroska_blocks_are_errors() {
        // Too short to contain anything.
        assert!(matroska_block_to_packet(&[]).is_err());
        assert!(matroska_block_to_packet(&[1, 2, 3]).is_err());

        // No blocks.
        assert!(matroska_block_to_packet(&4096u32.to_le_bytes()).is_err());

        // A block with a truncated header.
        let mut frame = Vec::new();
        frame.extend(4096u32.to_le_bytes());
        frame.extend((INITIAL | FINAL).to_le_bytes());
        assert!(matroska_block_to_packet(&frame).is_err());

        // A multichannel block with a size larger than the frame.
        let mut frame = Vec::new();
        frame.extend(4096u32.to_le_bytes());
        frame.extend(INITIAL.to_le_bytes());
        frame.extend(0u32.to_le_bytes());
        frame.extend(u32::MAX.to_le_bytes());
        frame.extend([0u8; 16]);
        assert!(matroska_block_to_packet(&frame).is_err());

        // A sub-block larger than the block.
        let mut frame = Vec::new();
        frame.extend(4096u32.to_le_bytes());
        frame.extend((INITIAL | FINAL).to_le_bytes());
        frame.extend(0u32.to_le_bytes());
        frame.extend([0x0a, 0x7f, 1, 2, 3, 4]);
        assert!(matroska_block_to_packet(&frame).is_err());
    }

    #[test]
    fn verify_sub_block_size_does_not_allocate() {
        // A large sub-block (the 3 byte size is in words) with no data must not allocate its
        // declared size.
        let mut reader = BufReader::new(&[0x8a, 0xff, 0xff, 0xff]);
        assert!(decode_sub_block(&mut reader).is_err());
    }

    const SINGLE: u32 = INITIAL | FINAL | 0x1;

    #[test]
    fn verify_seek_index_is_bounded_and_sorted() {
        let mut index = SeekIndex::new();
        for i in 0..(MAX_INDEX_ENTRIES as i64 * 50) {
            index.insert(IndexEntry { ts: i * 1000, pos: i as u64, wvc_pos: 0 });
            assert!(index.entries.len() <= MAX_INDEX_ENTRIES);
        }
        assert!(index.entries.windows(2).all(|w| w[0].ts + index.min_gap / 2 <= w[1].ts));
        assert_eq!(index.entries[0].ts, 0);

        // The entry found is the last one at or before the timestamp.
        let target = 123_456_789;
        let e = index.find(target).unwrap();
        assert!(e.ts <= target);
        let next = index.entries.iter().find(|n| n.ts > target).unwrap();
        assert!(next.ts > target && next.ts > e.ts);
        assert!(index.find(-1).is_none());

        // Entries are never inserted out of order (e.g. while rescanning after a seek).
        let len = index.entries.len();
        index.insert(IndexEntry { ts: 5, pos: 0, wvc_pos: 0 });
        assert_eq!(index.entries.len(), len);
    }

    /// A main block header.
    fn main_header(index: u64, block_samples: u32, flags: u32) -> Header {
        Header {
            ck_size: 0,
            version: 0x410,
            block_index_u8: (index >> 32) as u8,
            total_samples_u8: 0,
            total_samples_u32: 0,
            block_index_u32: index as u32,
            block_samples,
            flags,
            crc: 0,
        }
    }

    /// A complete `.wvc` block: header, a shaping sub-block and a correction bitstream.
    fn wvc_block(index: u64, block_samples: u32, flags: u32, crc: u32) -> Vec<u8> {
        let payload = [sub_block(0x07, &[7, 7]), sub_block(0x0b, &[1, 2, 3, 4])].concat();

        let mut block = Vec::new();
        block.extend(b"wvpk");
        block.extend((24 + payload.len() as u32).to_le_bytes());
        block.extend(0x410u16.to_le_bytes());
        block.push((index >> 32) as u8);
        block.push(0);
        block.extend(0u32.to_le_bytes());
        block.extend((index as u32).to_le_bytes());
        block.extend(block_samples.to_le_bytes());
        block.extend(flags.to_le_bytes());
        block.extend(crc.to_le_bytes());
        block.extend(payload);
        block
    }

    fn correction(blocks: &[Vec<u8>]) -> Correction<'static> {
        let data = blocks.concat();
        Correction::new(MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default()))
            .unwrap()
    }

    fn wvc(index: u64, block_samples: u32, flags: u32) -> WvcBlock {
        WvcBlock { block_index: index, block_samples, flags, crc: 0, parts: BlockParts::default() }
    }

    #[test]
    fn verify_wvc_header_matching() {
        let wv = main_header(1000, 500, SINGLE);

        // Same position and flags: a match.
        assert_eq!(match_wvc_header(&wv, &wvc(1000, 500, SINGLE)), 0);
        // An earlier correction block is stale, a later one is for a later block.
        assert_eq!(match_wvc_header(&wv, &wvc(500, 500, SINGLE)), 1);
        assert_eq!(match_wvc_header(&wv, &wvc(1500, 500, SINGLE)), -1);
        // Same index but another length is not the same block either.
        assert_eq!(match_wvc_header(&wv, &wvc(1000, 400, SINGLE)), -1);
        // Block indices have 40 bits: an index that wrapped is "earlier".
        let wv = main_header(5, 500, SINGLE);
        assert_eq!(match_wvc_header(&wv, &wvc(0xff_ffff_fff0, 500, SINGLE)), 1);

        // The streams of a multichannel group are told apart by their position flags: the
        // correction block of the first stream (INITIAL) is stale for the last (FINAL).
        let wv_last = main_header(0, 500, FINAL | 0x1);
        assert_eq!(match_wvc_header(&wv_last, &wvc(0, 500, INITIAL | 0x1)), 1);
        let wv_first = main_header(0, 500, INITIAL | 0x1);
        assert_eq!(match_wvc_header(&wv_first, &wvc(0, 500, FINAL | 0x1)), -1);
    }

    #[test]
    fn verify_correction_block_lookup() {
        let mut c = correction(&[
            wvc_block(0, 1000, SINGLE, 0xa0),
            // Blocks without samples carry nothing to correct.
            wvc_block(1000, 0, SINGLE, 0xdead),
            wvc_block(1000, 1000, SINGLE, 0xa1),
            wvc_block(3000, 1000, SINGLE, 0xa3),
        ]);

        // The first block is matched.
        let block = c.block_for(&main_header(0, 1000, SINGLE)).unwrap();
        assert_eq!(block.crc, 0xa0);
        assert_eq!(block.parts.wvc, [1, 2, 3, 4]);
        assert_eq!(block.parts.shaping, [7, 7]);

        // The second block is matched; the empty block is skipped.
        assert_eq!(c.block_for(&main_header(1000, 1000, SINGLE)).unwrap().crc, 0xa1);

        // There is no correction block for the third block; the next one is kept for later.
        assert!(c.block_for(&main_header(2000, 1000, SINGLE)).is_none());
        assert_eq!(c.block_for(&main_header(3000, 1000, SINGLE)).unwrap().crc, 0xa3);

        // That was the last one.
        assert!(c.block_for(&main_header(4000, 1000, SINGLE)).is_none());
        assert!(c.done);

        // After a restart (a seek) the stream is read from the beginning again.
        c.restart().unwrap();
        assert_eq!(c.block_for(&main_header(0, 1000, SINGLE)).unwrap().crc, 0xa0);

        // A mark restores the position of the stream and the block that was read ahead.
        let mark = c.mark();
        assert!(c.block_for(&main_header(2000, 1000, SINGLE)).is_none());
        assert_eq!(c.block_for(&main_header(3000, 1000, SINGLE)).unwrap().crc, 0xa3);
        c.restore(mark).unwrap();
        assert_eq!(c.block_for(&main_header(1000, 1000, SINGLE)).unwrap().crc, 0xa1);
    }

    #[test]
    fn verify_correction_stream_skips_garbage_and_stale_blocks() {
        // Junk, then a correction block that is stale for the main block.
        let mut data = vec![0u8; 13];
        data.extend(wvc_block(0, 1000, SINGLE, 1));
        data.extend([0xff; 7]);
        data.extend(wvc_block(1000, 1000, SINGLE, 2));

        let mut c = correction(&[data]);
        assert_eq!(c.block_for(&main_header(1000, 1000, SINGLE)).unwrap().crc, 2);

        // A stream without any block cannot be a correction stream.
        assert!(Correction::new(MediaSourceStream::new(
            Box::new(std::io::Cursor::new(vec![0u8; 64])),
            Default::default(),
        ))
        .is_err());
    }

    #[test]
    fn verify_packet_carries_the_correction_block() {
        let main = BlockParts {
            terms: vec![1, 2],
            wvx: vec![9, 9, 9, 9, 8, 8],
            audio: vec![5, 5, 5, 5],
            ..Default::default()
        };
        let correction = WvcBlock {
            block_index: 0,
            block_samples: 10,
            flags: SINGLE,
            crc: 0x1234_5678,
            parts: BlockParts {
                shaping: vec![7, 7],
                wvc: vec![1, 2, 3, 4],
                wvx: vec![6, 6, 6, 6, 6, 6],
                wvx_new: true,
                ..Default::default()
            },
        };

        let with = serialise_stream_packet(SINGLE, 10, 0xaaaa, &main, Some(&correction));
        let without = serialise_stream_packet(SINGLE, 10, 0xaaaa, &main, None);

        let field = |p: &[u8], i: usize| u32::from_le_bytes(p[4 * i..4 * i + 4].try_into().unwrap());

        // terms, wvx, shaping, wvc, wvc_wvx
        assert_eq!([field(&with, 3), field(&with, 10)], [2, 6]);
        assert_eq!([field(&with, 11), field(&with, 12), field(&with, 13)], [2, 4, 6]);
        assert_eq!(field(&with, 14), 0x1234_5678);
        assert_eq!(field(&with, 15), EXT_WVC_WVX_NEW);
        assert_eq!(
            &with[STREAM_HDR..],
            &[1, 2, 9, 9, 9, 9, 8, 8, 7, 7, 1, 2, 3, 4, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5]
        );

        // Without a correction block the correction fields are empty.
        assert_eq!([field(&without, 11), field(&without, 12), field(&without, 13)], [0, 0, 0]);
        assert_eq!([field(&without, 14), field(&without, 15)], [0, 0]);
        assert_eq!(&without[STREAM_HDR..], &[1, 2, 9, 9, 9, 9, 8, 8, 5, 5, 5, 5]);

        // A correction block that has no correction bitstream is of no use.
        let mut useless = correction.clone();
        useless.parts.wvc.clear();
        assert_eq!(serialise_stream_packet(SINGLE, 10, 0xaaaa, &main, Some(&useless)), without);
    }
}
