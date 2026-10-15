// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};

use log::{debug, info, warn};

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_S16LE, CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8,
    CODEC_ID_WMALOSSLESS, CODEC_ID_WMAPRO, CODEC_ID_WMAV1, CODEC_ID_WMAV2, CODEC_ID_WMAVOICE,
};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::TrackFlags;
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::packet::Packet;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use crate::objects::{AudioStream, HEADER_OBJECT, Header, read_header, read_simple_index};
use crate::packets::{PacketReader, StreamState, Unit};

const ASF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"ASF ")),
    short_name: "asf",
    long_name: "Advanced Systems Format (ASF/WMA)",
};

/// The WAVE format tags of the supported codecs (RFC 2361, and the ASF specification, 10.1).
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_WMAVOICE9: u16 = 0x000a;
const WAVE_FORMAT_WMAV1: u16 = 0x0160;
const WAVE_FORMAT_WMAV2: u16 = 0x0161;
const WAVE_FORMAT_WMAPRO: u16 = 0x0162;
const WAVE_FORMAT_WMALOSSLESS: u16 = 0x0163;

/// How the duration of a packet is estimated.
#[derive(Copy, Clone, Debug)]
enum DurationKind {
    /// Every packet has the same duration in samples (WMA, whose packets are blocks of
    /// `nBlockAlign` bytes).
    Fixed(u64),
    /// The duration is the packet length over the number of bytes per frame (PCM).
    PerFrame(u64),
}

impl DurationKind {
    fn dur(&self, len: usize) -> u64 {
        match *self {
            DurationKind::Fixed(dur) => dur,
            DurationKind::PerFrame(frame_bytes) => len as u64 / frame_bytes.max(1),
        }
    }
}

/// The timing information of a track.
#[derive(Copy, Clone, Debug)]
struct TrackTiming {
    stream: u8,
    rate: u32,
    dur: DurationKind,
}

/// A seek index entry: a presentation time (in milliseconds, after the preroll is removed) and
/// the position of a data packet.
type IndexEntry = (i64, u64);

/// The log2 of the WMA frame length for a sample rate, from FFmpeg's `ff_wma_get_frame_len_bits`
/// (`libavcodec/wma_common.c`) with `decode_flags` of 0. `version` is 1 for WMA v1, else 2.
fn wma_frame_len_bits(sample_rate: u32, version: u32) -> u32 {
    if sample_rate <= 16000 {
        9
    }
    else if sample_rate <= 22050 || (sample_rate <= 32000 && version == 1) {
        10
    }
    else if sample_rate <= 48000 || version < 3 {
        11
    }
    else if sample_rate <= 96000 {
        12
    }
    else {
        13
    }
}

/// Converts milliseconds to a timestamp in `1 / rate` units, rounding to nearest.
fn ms_to_ts(ms: i64, rate: u32) -> i64 {
    let n = i128::from(ms) * i128::from(rate);
    // Round half up.
    (n * 2 + 1000).div_euclid(2000) as i64
}

/// Converts a timestamp in `1 / rate` units to milliseconds, rounding down.
fn ts_to_ms(ts: i64, rate: u32) -> i64 {
    (i128::from(ts) * 1000).div_euclid(i128::from(rate.max(1))) as i64
}

/// Maps an audio stream to a track and its timing, or `None` if the codec is not supported.
fn make_track(stream: &AudioStream) -> Option<(Track, TrackTiming)> {
    let rate = stream.sample_rate;

    if rate == 0 || stream.channels == 0 {
        warn!("asf: ignoring audio stream {} with an invalid format", stream.number);
        return None;
    }

    let time_base = TimeBase::try_from_recip(rate)?;

    let channels = stream
        .channel_mask
        .and_then(Position::from_wave_channel_mask)
        .filter(|pos| pos.bits().count_ones() == u32::from(stream.channels))
        .or_else(|| Position::from_wave_channel_count(u32::from(stream.channels)))?;

    let mut params = AudioCodecParameters::new();
    params.with_sample_rate(rate).with_channels(Channels::Positioned(channels));

    let tag = stream.format_tag;
    let block_align = u64::from(stream.block_align);

    let (dur, delay) = match tag {
        WAVE_FORMAT_WMAV1
        | WAVE_FORMAT_WMAV2
        | WAVE_FORMAT_WMAPRO
        | WAVE_FORMAT_WMALOSSLESS
        | WAVE_FORMAT_WMAVOICE9 => {
            let codec = match tag {
                WAVE_FORMAT_WMAV1 => CODEC_ID_WMAV1,
                WAVE_FORMAT_WMAV2 => CODEC_ID_WMAV2,
                WAVE_FORMAT_WMAPRO => CODEC_ID_WMAPRO,
                WAVE_FORMAT_WMALOSSLESS => CODEC_ID_WMALOSSLESS,
                _ => CODEC_ID_WMAVOICE,
            };
            params.for_codec(codec).with_extra_data(stream.wave_format.clone());

            // The nominal duration of a block: its share of the bit rate.
            let dur = if stream.avg_bytes_per_sec > 0 {
                (u128::from(block_align) * u128::from(rate) / u128::from(stream.avg_bytes_per_sec))
                    as u64
            }
            else {
                0
            };

            // The overlap-add of WMA v1 and v2 delays the output by two frames (FFmpeg
            // `wmadec.c`: `avctx->delay = s->frame_len * 2`).
            let delay = match tag {
                WAVE_FORMAT_WMAV1 => Some(2u32 << wma_frame_len_bits(rate, 1)),
                WAVE_FORMAT_WMAV2 => Some(2u32 << wma_frame_len_bits(rate, 2)),
                _ => None,
            };

            (DurationKind::Fixed(dur), delay)
        }
        WAVE_FORMAT_PCM => {
            let bits = u32::from(stream.bits_per_sample);
            let codec = match bits {
                8 => CODEC_ID_PCM_U8,
                16 => CODEC_ID_PCM_S16LE,
                24 => CODEC_ID_PCM_S24LE,
                32 => CODEC_ID_PCM_S32LE,
                _ => {
                    warn!("asf: ignoring PCM stream {} with {bits} bits per sample", stream.number);
                    return None;
                }
            };
            params.for_codec(codec).with_bits_per_sample(bits).with_bits_per_coded_sample(bits);

            let frame_bytes = u64::from(stream.channels) * u64::from(bits / 8);
            (DurationKind::PerFrame(frame_bytes), None)
        }
        _ => {
            warn!(
                "asf: ignoring audio stream {} with unsupported format tag {tag:#06x}",
                stream.number
            );
            return None;
        }
    };

    let mut track = Track::new(u32::from(stream.number));

    track
        .with_codec_params(CodecParameters::Audio(params))
        .with_time_base(time_base)
        .with_start_ts(Timestamp::new(0));

    if let Some(delay) = delay {
        track.with_delay(delay);
    }

    Some((track, TrackTiming { stream: stream.number, rate, dur }))
}

/// An Advanced Systems Format (ASF) demuxer. See the crate documentation for the conventions.
pub struct AsfReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    timing: Vec<TrackTiming>,
    media_info: MediaInfo,
    metadata: MetadataLog,
    packets: PacketReader,
    /// Media objects that have been read but not returned.
    ready: VecDeque<Unit>,
    /// The preroll in milliseconds, subtracted from all presentation times.
    preroll_ms: i64,
    /// The position of the first data packet.
    data_pos: u64,
    data_end: Option<u64>,
    /// The size of a data packet, if all data packets have the same size.
    packet_size: Option<u32>,
    seekable: bool,
    eof: bool,
    /// The index period in milliseconds.
    index_period_ms: i64,
    /// Entries built while reading.
    scanned: Vec<IndexEntry>,
    /// Entries of the Simple Index Object, if it was loaded.
    simple: Vec<IndexEntry>,
    simple_loaded: bool,
    /// The timestamp of the last packet returned for each track.
    last_ts: Vec<Option<i64>>,
}

impl<'s> AsfReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let Header { file, streams, metadata, data_pos, data_end } = match read_header(&mut mss) {
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return decode_error("asf: header is truncated");
            }
            other => other?,
        };

        let preroll_ms = file.map_or(0, |f| f.preroll_ms.min(i64::MAX as u64) as i64);

        let mut tracks = Vec::new();
        let mut timing = Vec::new();
        let mut states = Vec::new();

        for stream in &streams {
            let Some((mut track, time)) = make_track(stream)
            else {
                continue;
            };

            // The play duration includes the preroll (spec 3.2).
            if let Some(file) = file.filter(|f| !f.is_broadcast() && f.play_duration > 0) {
                let play_ms = (file.play_duration / 10_000).min(i64::MAX as u64) as i64;
                let frames = ms_to_ts((play_ms - preroll_ms).max(0), time.rate).max(0) as u64;
                track.with_num_frames(frames).with_duration(Duration::new(frames));
            }

            if tracks.is_empty() {
                track.with_flags(TrackFlags::DEFAULT);
            }

            // WMA codecs consume exactly one block of `nBlockAlign` bytes per packet.
            let is_wma = matches!(
                stream.format_tag,
                WAVE_FORMAT_WMAV1
                    | WAVE_FORMAT_WMAV2
                    | WAVE_FORMAT_WMAPRO
                    | WAVE_FORMAT_WMALOSSLESS
                    | WAVE_FORMAT_WMAVOICE9
            );
            let block_len = if is_wma { usize::from(stream.block_align) } else { 0 };

            states.push(StreamState::new(stream.number, stream.spread, block_len));
            timing.push(time);
            tracks.push(track);
        }

        if tracks.is_empty() {
            return unsupported_error("asf: no supported audio streams");
        }

        let (min_size, max_size) = file.map_or((0, 0), |f| (f.min_packet_size, f.max_packet_size));
        let packet_size = file.and_then(|f| f.fixed_packet_size());

        let mut log = opts.external_data.metadata.unwrap_or_default();
        if let Some(revision) = metadata {
            log.push(revision);
        }

        let seekable = mss.is_seekable();

        let mut reader = AsfReader {
            media_info: MediaInfo::from_tracks(&tracks),
            last_ts: vec![None; tracks.len()],
            reader: mss,
            tracks,
            timing,
            metadata: log,
            packets: PacketReader::new(min_size, max_size, data_end, states),
            ready: VecDeque::new(),
            preroll_ms,
            data_pos,
            data_end,
            packet_size,
            seekable,
            eof: false,
            index_period_ms: i64::from(opts.seek_index_fill_period_ms),
            scanned: Vec::new(),
            simple: Vec::new(),
            simple_loaded: false,
        };

        if opts.prebuild_seek_index && seekable {
            reader.prebuild_index()?;
        }

        Ok(reader)
    }

    /// Reads all data packets to build the seek index, then returns to the first data packet.
    fn prebuild_index(&mut self) -> Result<()> {
        info!("asf: scanning data packets to build the seek index");

        while self.next_unit()?.is_some() {}

        self.rewind_to(self.data_pos)
    }

    /// Positions the stream at a data packet and discards all reading state.
    fn rewind_to(&mut self, pos: u64) -> Result<()> {
        self.reader.seek(SeekFrom::Start(pos))?;
        self.packets.reset();
        self.ready.clear();
        self.eof = false;
        Ok(())
    }

    /// Returns the index of the track of a stream number.
    fn track_index(&self, stream: u8) -> Option<usize> {
        self.timing.iter().position(|t| t.stream == stream)
    }

    /// The timestamp of a media unit in its track's timebase.
    fn unit_ts(&self, timing: &TrackTiming, unit: &Unit) -> (i64, u64) {
        let dur = timing.dur.dur(unit.data.len());
        let base = ms_to_ts(i64::from(unit.time_ms) - self.preroll_ms, timing.rate);
        (base.saturating_add((u64::from(unit.index) * dur) as i64), dur)
    }

    /// Returns the next media unit from any stream, reading data packets as necessary.
    fn next_unit(&mut self) -> Result<Option<Unit>> {
        loop {
            if let Some(unit) = self.ready.pop_front() {
                self.note_unit(&unit);
                return Ok(Some(unit));
            }

            if self.eof {
                return Ok(None);
            }

            if !self.packets.read_packet(&mut self.reader, &mut self.ready)? {
                self.eof = true;
            }
        }
    }

    /// Adds the unit to the seek index if there is no entry near it.
    fn note_unit(&mut self, unit: &Unit) {
        if unit.index != 0 || !self.seekable {
            return;
        }

        let ms = i64::from(unit.time_ms) - self.preroll_ms;

        let due = match self.scanned.last() {
            Some(&(last_ms, last_pos)) => {
                ms >= last_ms.saturating_add(self.index_period_ms.max(1)) && unit.pos > last_pos
            }
            None => true,
        };

        if due {
            self.scanned.push((ms, unit.pos));
        }
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        loop {
            let Some(unit) = self.next_unit()?
            else {
                return Ok(None);
            };

            let Some(idx) = self.track_index(unit.stream)
            else {
                continue;
            };

            let timing = self.timing[idx];
            let (ts, dur) = self.unit_ts(&timing, &unit);

            self.last_ts[idx] = Some(ts);

            return Ok(Some(Packet::new(
                u32::from(unit.stream),
                Timestamp::new(ts),
                Duration::new(dur),
                unit.data,
            )));
        }
    }

    /// Loads the Simple Index Object, which follows the Data Object (spec 6.1). Entries are
    /// stored with the position of the previous entry: a media object may start in the data
    /// packet before the one an entry names, and starting early costs only a little reading.
    fn load_simple_index(&mut self) -> Result<()> {
        self.simple_loaded = true;

        let (Some(data_end), Some(packet_size), Some(len)) =
            (self.data_end, self.packet_size, self.reader.byte_len())
        else {
            return Ok(());
        };

        let found = match read_simple_index(&mut self.reader, data_end, len) {
            Ok(found) => found,
            Err(err) => {
                debug!("asf: ignoring unreadable simple index: {err}");
                None
            }
        };

        let Some((interval, packets)) = found
        else {
            return Ok(());
        };

        debug!("asf: using the simple index object with {} entries", packets.len());

        let mut prev_pos = self.data_pos;
        let mut simple = Vec::with_capacity(packets.len());

        for (i, packet) in packets.into_iter().enumerate() {
            let pos = self.data_pos.saturating_add(u64::from(packet) * u64::from(packet_size));
            let ms = (i as u128 * u128::from(interval) / 10_000).min(i64::MAX as u128) as i64
                - self.preroll_ms;

            // Entries beyond the data, or out of order, are not trusted.
            if pos >= data_end || pos < prev_pos {
                continue;
            }

            simple.push((ms, prev_pos));
            prev_pos = pos;
        }

        self.simple = simple;
        Ok(())
    }

    /// Finds the data packet to start scanning from for a target time. The second value is true
    /// if the position came from an index.
    fn start_for(&self, target_ms: i64) -> (u64, bool) {
        let find = |index: &[IndexEntry]| {
            let n = index.partition_point(|&(ms, _)| ms <= target_ms);
            n.checked_sub(1).and_then(|i| index.get(i)).copied()
        };

        match (find(&self.scanned), find(&self.simple)) {
            (Some(a), Some(b)) => (if a.0 >= b.0 { a.1 } else { b.1 }, true),
            (Some(e), None) | (None, Some(e)) => (e.1, true),
            (None, None) => (self.data_pos, false),
        }
    }

    fn seek_inner(&mut self, to: SeekTo) -> Result<SeekedTo> {
        let (idx, required_ts) = match to {
            SeekTo::Timestamp { ts, track_id } => {
                let idx = self
                    .tracks
                    .iter()
                    .position(|t| t.id == track_id)
                    .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                (idx, ts)
            }
            SeekTo::Time { time, track_id } => {
                let idx = match track_id {
                    Some(id) => self.tracks.iter().position(|t| t.id == id),
                    None => Some(0),
                }
                .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                let tb = self.tracks[idx]
                    .time_base
                    .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                let ts =
                    tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;
                (idx, ts)
            }
        };

        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let timing = self.timing[idx];
        let stream = timing.stream;
        let required = required_ts.get();

        debug!("asf: seeking to ts={required} on stream {stream}");

        if self.seekable {
            if !self.simple_loaded {
                self.load_simple_index()?;
            }

            let target_ms = ts_to_ms(required, timing.rate);
            let (pos, from_index) = self.start_for(target_ms);

            self.rewind_to(pos)?;

            match self.scan_to(&timing, required, !from_index)? {
                Scan::Found(landing) => return self.finish_seek(idx, required_ts, landing),
                // The index led past the target: scan from the start.
                Scan::Before => (),
                Scan::Beyond => return seek_error(SeekErrorKind::OutOfRange),
            }

            self.rewind_to(self.data_pos)?;
        }
        else if self.last_ts[idx].is_some_and(|last| required < last) {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        match self.scan_to(&timing, required, true)? {
            Scan::Found(landing) => self.finish_seek(idx, required_ts, landing),
            Scan::Before | Scan::Beyond => seek_error(SeekErrorKind::OutOfRange),
        }
    }

    fn finish_seek(&mut self, idx: usize, required_ts: Timestamp, to: Landing) -> Result<SeekedTo> {
        // The next packets are the landing unit, the first unit after the target (if any), and
        // everything read after it.
        if let Some(next) = to.next {
            self.ready.push_front(next);
        }
        let actual_ts = to.ts;
        self.ready.push_front(to.unit);
        self.last_ts[idx] = None;

        Ok(SeekedTo {
            track_id: self.tracks[idx].id,
            required_ts,
            actual_ts: Timestamp::new(actual_ts),
        })
    }

    /// Reads units until one of the stream starts after `required`, and returns the last unit that
    /// starts at or before it. If every unit starts after `required`, then the first unit is
    /// returned when the scan started at the first data packet (`at_start`), otherwise `Before`.
    fn scan_to(&mut self, timing: &TrackTiming, required: i64, at_start: bool) -> Result<Scan> {
        let mut best: Option<(Unit, i64, u64)> = None;

        loop {
            let Some(unit) = self.next_unit()?
            else {
                // The end of the data: the target is only valid within the last unit.
                return Ok(match best {
                    Some((unit, ts, dur)) if required <= ts.saturating_add(dur as i64) => {
                        Scan::Found(Landing { unit, ts, next: None })
                    }
                    _ => Scan::Beyond,
                });
            };

            if unit.stream != timing.stream {
                continue;
            }

            let (ts, dur) = self.unit_ts(timing, &unit);

            if ts <= required {
                best = Some((unit, ts, dur));
            }
            else {
                return Ok(match best {
                    Some((best, ts, _)) => {
                        Scan::Found(Landing { unit: best, ts, next: Some(unit) })
                    }
                    // The target is before the first unit that was read.
                    None if at_start => Scan::Found(Landing { unit, ts, next: None }),
                    None => Scan::Before,
                });
            }
        }
    }
}

/// The result of scanning for a seek target.
enum Scan {
    Found(Landing),
    /// Every unit read started after the target.
    Before,
    /// The target is past the end of the data.
    Beyond,
}

struct Landing {
    unit: Unit,
    ts: i64,
    next: Option<Unit>,
}

impl Scoreable for AsfReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut guid = [0u8; 16];

        if src.read_buf_exact(&mut guid).is_err() || guid != HEADER_OBJECT {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for AsfReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AsfReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ASF_FORMAT_INFO,
            &["wma", "wmv", "asf"],
            &["audio/x-ms-wma", "video/x-ms-asf", "audio/x-ms-asf"],
            &[&HEADER_OBJECT]
        )]
    }
}

impl FormatReader for AsfReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ASF_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.next_packet_inner()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        None
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        self.seek_inner(to)
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
