// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::audio::well_known::CODEC_ID_WAVPACK;
use symphonia_core::support_format;

use symphonia_core::audio::layouts;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_WAVPACK;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataBuilder, MetadataInfo, MetadataLog, Tag, well_known};

use log::debug;

mod v4v5;
use v4v5::Correction;
pub(crate) use v4v5::matroska_block_to_packet;

const WAVPACK_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_WAVPACK, short_name: "wavpack", long_name: "WavPack" };

const STREAM_MARKER: [u8; 4] = *b"wvpk";
const RIFF_MARKER: [u8; 4] = *b"RIFF";
const WAVE_MARKER: [u8; 4] = *b"WAVE";

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
    /// (v4/v5) The block index of the first sample of the stream: the stream's timeline starts
    /// there (a stream cut out of a longer one does not start at 0).
    initial_index: u64,
    /// (v4/v5) The timestamps per sample (frame) of the stream: 8 for DSD, which counts bits and
    /// has bytes (8 bits each) as its samples, 1 otherwise.
    ts_scale: u64,
    /// A packet has been read.
    started: bool,
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
            if let Err(err) =
                reader.attach_correction(MediaSourceStream::new(sidecar, Default::default()))
            {
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
        if self.started {
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
        }
        else {
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
            let chunk_id = mss.read_quad_bytes()?;
            let chunk_size = mss.read_u32()?;

            match &chunk_id {
                b"fmt " => {
                    if chunk_size < 16 {
                        return decode_error("wavpack v3: fmt chunk too small");
                    }
                    let format_tag = mss.read_u16()?;
                    let num_channels = mss.read_u16()?;
                    let sample_rate = mss.read_u32()?;
                    let _bytes_per_sec = mss.read_u32()?;
                    let block_align = mss.read_u16()?;
                    let bits_per_sample = mss.read_u16()?;

                    let extra = chunk_size - 16;
                    if extra > 0 {
                        mss.ignore_bytes(extra as u64)?;
                    }
                    if chunk_size % 2 != 0 {
                        let _ = mss.read_u8()?;
                    }

                    if format_tag != 1 {
                        return unsupported_error("wavpack v3: non-PCM fmt");
                    }
                    if num_channels == 0 || num_channels > 2 {
                        return decode_error("wavpack v3: unsupported channel count");
                    }
                    let bytes_per_sample = block_align / num_channels;
                    wav_header = Some(WaveHeader3 {
                        sample_rate,
                        num_channels,
                        bits_per_sample,
                        bytes_per_sample,
                    });
                }

                b"smpl" => {
                    parse_smpl_chunk(&mut mss, chunk_size, &mut meta_builder)?;
                    if chunk_size % 2 != 0 {
                        let _ = mss.read_u8()?;
                    }
                }

                b"cue " => {
                    parse_cue_chunk(&mut mss, chunk_size, &mut meta_builder)?;
                    if chunk_size % 2 != 0 {
                        let _ = mss.read_u8()?;
                    }
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
        }
        else {
            layouts::CHANNEL_LAYOUT_STEREO
        };

        let bits_per_sample = wav.bits_per_sample as u32;
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
            initial_index: 0,
            ts_scale: 1,
            started: false,
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
        self.started = true;
        let pos = self.reader.pos();
        let wvc_pos = self.correction.as_ref().map_or(0, |c| c.resume_pos());
        let pkt = match &self.format_version {
            FormatVersion::V3 { num_channels, bytes_per_sample } => {
                let (ch, bps) = (*num_channels, *bytes_per_sample);
                self.next_packet_v3(ch, bps)
            }
            FormatVersion::V4V5 => self.next_packet_v4v5(),
        }?;
        if let Some(pkt) = &pkt {
            self.index.insert(IndexEntry { ts: pkt.pts.get(), pos, wvc_pos });
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
                if let (Some(correction), Some(mark)) = (self.correction.as_mut(), correction_mark)
                {
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
        }
        else {
            // Real WavPack 3.97 files: ck_size == header only; compressed audio
            // follows the block header and extends to the next "wvpk" or EOF.
            let audio = read_v3_audio(&mut self.reader)?;
            pkt_data.extend_from_slice(&audio);
        }

        let n_samples = hdr.total_samples.max(0) as u64;
        let ts = self.next_packet_ts;
        self.next_packet_ts += n_samples as i64;

        Ok(Some(Packet::new(0, Timestamp::new(ts), Duration::new(n_samples), pkt_data)))
    }
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
    const LOOP_ENTRY: u32 = 24;

    if chunk_size < SAMPLER_HDR {
        debug!("wavpack: smpl chunk too small ({})", chunk_size);
        mss_skip(source, chunk_size as u64)?;
        return Ok(());
    }

    let _manufacturer = source.read_u32()?;
    let _product = source.read_u32()?;
    let sample_period = source.read_u32()?;
    let midi_note = source.read_u32()?;
    let pitch_fraction = source.read_u32()?;
    let _smpte_format = source.read_u32()?;
    let _smpte_offset = source.read_u32()?;
    let num_loops = source.read_u32()?;
    let _sampler_data = source.read_u32()?;

    builder.add_tag(Tag::new_from_parts("WavPack/MidiNote", midi_note, None));
    builder.add_tag(Tag::new_from_parts("WavPack/PitchFraction", pitch_fraction, None));
    // Sample period (ns/sample) is used by Hauptwerk for fine tuning when it
    // disagrees with the format chunk's sample rate. Skip the trivial case
    // (0 = "unspecified") so the tag set stays meaningful.
    if sample_period != 0 {
        builder.add_tag(Tag::new_from_parts("WavPack/SamplePeriod", sample_period, None));
    }
    builder.add_tag(Tag::new_from_parts("WavPack/LoopCount", num_loops, None));

    let loops_size = num_loops.saturating_mul(LOOP_ENTRY);
    let remaining = chunk_size.saturating_sub(SAMPLER_HDR);

    if loops_size > remaining {
        debug!("wavpack: smpl loop count exceeds chunk size");
        mss_skip(source, remaining as u64)?;
        return Ok(());
    }

    for i in 0..num_loops {
        let _id = source.read_u32()?;
        let loop_type = source.read_u32()?;
        let start = source.read_u32()?;
        let end = source.read_u32()?;
        let fraction = source.read_u32()?;
        let play_cnt = source.read_u32()?;
        // Hauptwerk uses Type (0=forward, 1=alternating, 2=reverse) and
        // PlayCount (0 = infinite). GrandOrgue only consults Start/End but
        // is fine with the extras being present.
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/Type"), loop_type, None));
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/Start"), start, None));
        builder.add_tag(Tag::new_from_parts(format!("WavPack/Loop{i}/End"), end, None));
        if fraction != 0 {
            builder.add_tag(Tag::new_from_parts(
                format!("WavPack/Loop{i}/Fraction"),
                fraction,
                None,
            ));
        }
        if play_cnt != 0 {
            builder.add_tag(Tag::new_from_parts(
                format!("WavPack/Loop{i}/PlayCount"),
                play_cnt,
                None,
            ));
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
    const CUE_HDR: u32 = 4;
    const CUE_ENTRY: u32 = 24;

    if chunk_size < CUE_HDR {
        debug!("wavpack: cue chunk too small");
        mss_skip(source, chunk_size as u64)?;
        return Ok(());
    }

    let num_cues = source.read_u32()?;
    let entries_sz = num_cues.saturating_mul(CUE_ENTRY);
    let remaining = chunk_size.saturating_sub(CUE_HDR);

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
        let name = source.read_u32()?;
        let _position = source.read_u32()?;
        let _fcc_chunk = source.read_u32()?;
        let _chunk_start = source.read_u32()?;
        let _block_start = source.read_u32()?;
        let sample_offset = source.read_u32()?;

        builder.add_tag(Tag::new_from_parts(format!("WavPack/Cue{i}/Name"), name, None));
        builder.add_tag(Tag::new_from_parts(
            format!("WavPack/Cue{i}/SampleOffset"),
            sample_offset,
            None,
        ));

        release_point = Some(match release_point {
            Some(prev) => prev.max(sample_offset),
            None => sample_offset,
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

/// Parse RIFF chunks out of a raw byte buffer (with or without a leading
/// `RIFF....WAVE` envelope) and feed `smpl` / `cue ` into the existing
/// chunk parsers via a Cursor-backed MediaSourceStream.
fn parse_riff_meta_bytes(buf: &[u8], builder: &mut MetadataBuilder) -> Result<()> {
    use symphonia_core::io::MediaSourceStreamOptions;

    let mut start = 0usize;
    if buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"WAVE" {
        start = 12;
    }
    else if buf.len() >= 4 && &buf[0..4] == b"WAVE" {
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
                if chunk_size % 2 != 0 {
                    let _ = mss.read_u8();
                }
            }
            b"cue " => {
                parse_cue_chunk(&mut mss, chunk_size, builder)?;
                if chunk_size % 2 != 0 {
                    let _ = mss.read_u8();
                }
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
                if mss.ignore_bytes(skip).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// V3 structures
// ---------------------------------------------------------------------------

struct WaveHeader3 {
    sample_rate: u32,
    num_channels: u16,
    bits_per_sample: u16,
    bytes_per_sample: u16,
}

struct Header3 {
    ck_size: i32,
    version: i16,
    bits: i16,
    flags: i16,
    shift: i16,
    total_samples: i32,
    crc: i32,
    crc2: i32,
    ext: [u8; 4],
    extra_bc: u8,
    extras: [u8; 3],
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
            let flags = reader.read_i16()?;
            let shift = reader.read_i16()?;
            let total_samples = reader.read_i32()?;
            let crc = reader.read_i32()?;
            let crc2 = reader.read_i32()?;
            let mut ext = [0u8; 4];
            reader.read_buf_exact(&mut ext)?;
            let extra_bc = reader.read_u8()?;
            let mut extras = [0u8; 3];
            reader.read_buf_exact(&mut extras)?;
            (flags, shift, total_samples, crc, crc2, ext, extra_bc, extras)
        }
        else {
            (0, 0, 0, 0, 0, [0u8; 4], 0u8, [0u8; 3])
        };

        Ok(Header3 {
            ck_size,
            version,
            bits,
            flags,
            shift,
            total_samples,
            crc,
            crc2,
            ext,
            extra_bc,
            extras,
        })
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
        }
        else {
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
