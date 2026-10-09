// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::support_format;

use symphonia_core::checksum::Crc16AnsiLe;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::{FORMAT_ID_MP1, FORMAT_ID_MP2, FORMAT_ID_MP3};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog, MetadataSideData};

use crate::common::{FrameHeader, MpegLayer};
use crate::header::{self, MAX_MPEG_FRAME_SIZE, MPEG_HEADER_LEN};
use crate::trailer;
use symphonia_metadata::id3v2;

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};

use log::{debug, info, warn};

const MP1_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP1, short_name: "mp1", long_name: "MPEG Audio Layer 1 Native" };

const MP2_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP2, short_name: "mp2", long_name: "MPEG Audio Layer 2 Native" };

const MP3_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP3, short_name: "mp3", long_name: "MPEG Audio Layer 3 Native" };

/// The number of MPEG frames before the frame containing the seek target that are always decoded
/// after a seek.
///
/// The output of a frame depends on the overlap-add of the previous granule, and on the state of the
/// polyphase synthesis filterbank, which holds up to 1 granule (Layer 3) or 1.33 frames (Layer 1)
/// of history. For the output of the target frame to be identical to that of a continuous decode,
/// the frames up-to 2 granules before it must be decoded correctly.
const SEEK_PREROLL_FRAMES: usize = 3;

/// The maximum number of frames of history retained while seeking. This must be larger than
/// `SEEK_PREROLL_FRAMES` plus the maximum number of frames the bit reservoir can span (up to 511
/// bytes of main data in the smallest possible frames).
const SEEK_HISTORY_FRAMES: usize = 64;

/// A seek begins scanning for the target frame at least this many frames before the target frame so
/// that enough history is available to find the frame where decoding must start.
const SEEK_SCAN_MARGIN_FRAMES: u64 = 32;

/// The number of bytes following the frame header that are read when scanning a frame (without
/// reading the whole frame) to inspect its side information and look for a Xing/Info/VBRI tag.
const SCAN_PREFIX_LEN: usize = 60;

/// The number of windows, and frames in each window, sampled spread throughout the stream to
/// estimate the number of frames of a stream with no Xing/Info/VBRI tag.
const ESTIMATE_WINDOWS: u64 = 32;
const ESTIMATE_WINDOW_FRAMES: u32 = 4;

/// MPEG1 and MPEG2 audio elementary stream reader.
///
/// `MpaReader` implements a demuxer for the MPEG1 and MPEG2 audio elementary stream.
///
/// # Seeking
///
/// MPEG audio frames carry no timestamps, so the position of the frame containing a timestamp can
/// only be found exactly by counting frames from a known position. The reader builds a sparse index
/// of known frame positions (one entry per `FormatOptions::seek_index_fill_period_ms`) as frames are
/// read or skipped. A seek starts from the nearest known position before the target, therefore, the
/// first seek to a position that is far from any known position may need to read the stream up to
/// that position. `FormatOptions::prebuild_seek_index` builds the index (and obtains an exact
/// duration for streams with no Xing/Info/VBRI tag) when the reader is created.
///
/// Both `SeekMode::Coarse` and `SeekMode::Accurate` always seek to an exact frame boundary, and
/// provide the frames before the target that are required to reproduce the exact output of a
/// continuous decode, including the Layer 3 bit reservoir and overlap, when the decoder is reset
/// after the seek.
pub struct MpaReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    next_packet_ts: Timestamp,
    /// The absolute byte position where the audio data ends (exclusive), if known. Tags appended
    /// after the audio data are excluded.
    data_end: Option<u64>,
    /// The length in bytes, including the header and excluding padding, of the frames of a
    /// free-format stream.
    free_len: Option<usize>,
    /// If true, the track's number of frames is exact (obtained from a Xing/Info/VBRI tag, or by
    /// scanning the stream). Otherwise, it is an estimate.
    is_duration_exact: bool,
    /// The duration of a MPEG frame.
    frame_dur: Duration,
    /// The index of known frame positions.
    seek_index: SeekIndex,
}

/// A frame position known to be at the exact timestamp.
#[derive(Copy, Clone)]
struct SeekPoint {
    ts: Timestamp,
    pos: u64,
}

/// A sparse index of the positions of MPEG frames.
struct SeekIndex {
    /// The minimum duration between index entries.
    period: Duration,
    /// The index entries, sorted by timestamp. The first entry is always the first packet.
    points: Vec<SeekPoint>,
}

impl SeekIndex {
    fn new(period: Duration, first: SeekPoint) -> Self {
        SeekIndex { period, points: vec![first] }
    }

    /// Insert a frame into the index if it is far enough past the last entry.
    fn insert(&mut self, ts: Timestamp, pos: u64) {
        let is_next = match self.points.last() {
            Some(last) => last.ts.checked_add(self.period).is_some_and(|next| ts >= next),
            None => true,
        };

        if is_next {
            self.points.push(SeekPoint { ts, pos });
        }
    }

    /// Get the last entry with a timestamp less-than or equal to the provided timestamp, or the
    /// first entry if there is none.
    fn lookup(&self, ts: Timestamp) -> SeekPoint {
        let idx = self.points.partition_point(|point| point.ts <= ts);
        self.points[idx.saturating_sub(1)]
    }
}

/// A MPEG frame, skipped over without being read entirely.
struct ScannedFrame {
    /// The position of the frame header.
    pos: u64,
    header: FrameHeader,
    /// The main_data_begin value in the side information (Layer 3 only, otherwise 0).
    main_data_begin: u16,
    /// The number of bytes of main data in the frame (Layer 3 only, otherwise 0).
    main_data_len: usize,
}

/// The number of bytes before a candidate MPEG audio frame that are searched for MPEG program
/// stream start codes.
const PS_LOOKBACK_LEN: usize = 4096;

/// Returns true if the buffered stream is an MPEG program stream (MPEG-PS). MPEG audio frames are
/// found inside the PES packets of such a stream, but the stream is not an MPEG audio stream: the
/// audio data is interleaved with PES headers, and so decoding it as one would produce garbage.
///
/// The stream is a program stream if either:
/// * it begins with a pack header (`00 00 01 BA`), if the start of the stream is still buffered, or
/// * a PS/PES start code (`00 00 01` followed by a pack, system, or stream id) is found in the
///   buffered bytes just before the current position. This catches the probe scanning ahead to
///   an MPEG audio frame inside a PES packet after the start of the stream was dropped from the
///   buffer. Frames of an elementary stream do not contain start codes in practice.
///
/// The position of the stream is restored.
fn is_mpeg_ps_stream(reader: &mut MediaSourceStream<'_>) -> bool {
    let pos = reader.pos();

    // Look at the start of the stream.
    if reader.seek_buffered(0) == 0 {
        let mut buf = [0u8; 5];

        let is_ps = reader.read_buf_exact(&mut buf).is_ok()
            && buf[..4] == [0x00, 0x00, 0x01, 0xba]
            // MPEG-2 PS: '01' marker bits. MPEG-1 PS: '0010' marker bits.
            && (buf[4] & 0xc0 == 0x40 || buf[4] & 0xf0 == 0x20);

        reader.seek_buffered(pos);

        if is_ps {
            return true;
        }
    }

    // Look at the bytes before the current position.
    let back = (pos as usize).min(PS_LOOKBACK_LEN);
    let mut buf = [0u8; PS_LOOKBACK_LEN];

    let is_ps = if back >= 4 && reader.seek_buffered(pos - back as u64) == pos - back as u64 {
        reader.read_buf_exact(&mut buf[..back]).is_ok()
            && buf[..back].windows(4).any(|w| {
                w[..3] == [0x00, 0x00, 0x01]
                    // Pack header, system header, program stream map, private stream 1, padding,
                    // private stream 2, audio streams, video streams.
                    && matches!(w[3], 0xba..=0xbf | 0xc0..=0xef)
            })
    }
    else {
        false
    };

    reader.seek_buffered(pos);

    is_ps
}

impl Scoreable for MpaReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        if is_mpeg_ps_stream(src.inner_mut()) {
            return Ok(Score::Unsupported);
        }

        // Read the sync word for the first (assumed) MPEG frame and try to parse it into a header.
        let sync1 = header::read_frame_header_word_no_sync(&mut src)?;

        let hdr1 = match header::parse_frame_header(sync1) {
            Ok(hdr1) => hdr1,
            // The header may be the header of a free-format frame. If so, the length of the frame
            // and the next frame header are found in the next few frames' worth of bytes.
            Err(err) if is_free_format_word(sync1) => {
                let window = read_free_format_window(&mut src, sync1);

                // The frame length can't be determined, this isn't a free-format stream.
                let Some(len) = header::find_free_format_len(&window)
                else {
                    return Err(err);
                };

                let hdr1 = header::parse_frame_header_free(sync1, Some(len))?;

                // Check the second frame header, which is in the window of bytes.
                let pos2 = MPEG_HEADER_LEN + hdr1.frame_size;

                return match window.get(pos2..pos2 + MPEG_HEADER_LEN) {
                    Some(bytes) => {
                        let sync2 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

                        if header::is_frame_header_word_synced(sync2)
                            && header::parse_frame_header_free(sync2, Some(len)).is_ok()
                        {
                            Ok(Score::Supported(255))
                        }
                        else {
                            Ok(Score::Unsupported)
                        }
                    }
                    None => Ok(Score::Supported(127)),
                };
            }
            Err(err) => return Err(err),
        };

        // Since the first header was parsed successfully, this may be a MPEG audio format. However,
        // if there is enough data left to read the frame body and another frame header, then a
        // higher confidence may be gained. If there is not enough data left, return a partially
        // confident score.
        if src.bytes_available() < (hdr1.frame_size + header::MPEG_HEADER_LEN) as u64 {
            return Ok(Score::Supported(127));
        }

        // Skip the frame body.
        src.ignore_bytes(hdr1.frame_size as u64)?;

        // Read another sync word for the second (assumed) MPEG frame.
        let sync2 = header::read_frame_header_word_no_sync(&mut src)?;

        // The second sync word should look like a sync word.
        if !header::is_frame_header_word_synced(sync2) {
            return Ok(Score::Unsupported);
        }

        // Try to parse the second sync word into a header.
        let _ = header::parse_frame_header(sync2)?;

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for MpaReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(MpaReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            // Layer 1
            support_format!(
                MP1_FORMAT_INFO,
                &["mp1"],
                &["audio/mpeg", "audio/mp1"],
                &[
                    &[0xff, 0xfe], // MPEG 1 with CRC
                    &[0xff, 0xff], // MPEG 1
                    &[0xff, 0xf6], // MPEG 2 with CRC
                    &[0xff, 0xf7], // MPEG 2
                    &[0xff, 0xe6], // MPEG 2.5 with CRC
                    &[0xff, 0xe7], // MPEG 2.5
                ]
            ),
            // Layer 2
            support_format!(
                MP2_FORMAT_INFO,
                &["mp2"],
                &["audio/mpeg", "audio/mp2"],
                &[
                    &[0xff, 0xfc], // MPEG 1 with CRC
                    &[0xff, 0xfd], // MPEG 1
                    &[0xff, 0xf4], // MPEG 2 with CRC
                    &[0xff, 0xf5], // MPEG 2
                    &[0xff, 0xe4], // MPEG 2.5 with CRC
                    &[0xff, 0xe5], // MPEG 2.5
                ]
            ),
            // Layer 3
            support_format!(
                MP3_FORMAT_INFO,
                &["mp3"],
                &["audio/mpeg", "audio/mp3"],
                &[
                    &[0xff, 0xfa], // MPEG 1 with CRC
                    &[0xff, 0xfb], // MPEG 1
                    &[0xff, 0xf2], // MPEG 2 with CRC
                    &[0xff, 0xf3], // MPEG 2
                    &[0xff, 0xe2], // MPEG 2.5 with CRC
                    &[0xff, 0xe3], // MPEG 2.5
                ]
            ),
        ]
    }
}

impl FormatReader for MpaReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        // Safety: MpaReader only supports/has audio tracks.
        match self.tracks[0]
            .codec_params
            .as_ref()
            .expect("track has codec params")
            .audio()
            .expect("codec params are audio")
            .codec
        {
            CODEC_ID_MP1 => &MP1_FORMAT_INFO,
            CODEC_ID_MP2 => &MP2_FORMAT_INFO,
            CODEC_ID_MP3 => &MP3_FORMAT_INFO,
            _ => unreachable!(),
        }
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let (header, data) = loop {
            // The audio data may end before the end of the stream if there are tags appended to it.
            if self.data_end.is_some_and(|end| self.reader.pos() >= end) {
                return Ok(None);
            }

            // Read the next MPEG frame.
            let (header, data) = match read_mpeg_frame(&mut self.reader, self.free_len) {
                Ok(frame) => frame,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // MPEG streams have no well-defined end, so when no more frames can be read,
                    // consider the stream ended.
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };

            // A frame that extends into the trailing tags is not a frame.
            if self.data_end.is_some_and(|end| self.reader.pos() > end) {
                return Ok(None);
            }

            // Check if the packet contains a Xing, Info, or VBRI tag.
            if is_maybe_info_tag(&data, &header) {
                if try_read_info_tag(&data, &header).is_some() {
                    // Discard the packet and tag since it was not at the start of the stream.
                    warn!("found an unexpected xing tag, discarding");
                    continue;
                }
            }
            else if is_maybe_vbri_tag(&data, &header)
                && try_read_vbri_tag(&data, &header).is_some()
            {
                // Discard the packet and tag since it was not at the start of the stream.
                warn!("found an unexpected vbri tag, discarding");
                continue;
            }

            break (header, data);
        };

        // The timestamp and duration for this packet.
        let pts = self.next_packet_ts;
        let dur = header.duration();

        // Remember the position of the packet.
        self.seek_index.insert(pts, self.reader.pos() - data.len() as u64);

        // Advance the next packet timestamp based on this packet's duration. If it saturates, then
        // it is not possible to read further.
        self.next_packet_ts = match self.next_packet_ts.checked_add(dur) {
            Some(ts) => ts,
            None => return Ok(None),
        };

        // Only trim the end of the stream if the duration is exact. Trimming to an estimated
        // duration would discard audio if the estimate was too short.
        let end_ts = if self.is_duration_exact {
            self.tracks[0]
                .num_frames
                .map(Duration::from)
                .and_then(|dur| dur.timestamp_from(Timestamp::ZERO))
        }
        else {
            None
        };

        // Build the packet.
        let packet =
            PacketBuilder::new().track_id(0).pts(pts).trimmed_dur(dur, end_ts).data(data).build();

        Ok(Some(packet))
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
        // Get the timestamp of the desired audio frame.
        let required_ts = match to {
            // Frame timestamp given.
            SeekTo::Timestamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the timebase.
            SeekTo::Time { time, .. } => {
                // The timebase is required to calculate the timestamp.
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                // If the timestamp overflows, the seek if out-of-range.
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        // Get the total duration of the track (excludes delay and padding).
        let dur_ts = self.tracks[0].num_frames.map(Duration::from);

        // Get the delay and padding values (or 0 if unknown).
        let delay = self.tracks[0].delay.unwrap_or(0);
        let padding = self.tracks[0].padding.unwrap_or(0);

        // Calculate the valid timestamp bounds of the track.
        let min_ts = Timestamp::from(-i64::from(delay));
        let max_ts = dur_ts
            .and_then(|dur| min_ts.checked_add(dur))
            .and_then(|dur| dur.checked_add(Duration::from(delay + padding)));

        // Ensure the seek position is within the bounds of the track.
        if required_ts < min_ts {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        else if let Some(max_ts) = max_ts {
            // An estimated duration may be too short, in which case the end of the stream
            // determines if the position is out-of-range.
            if self.is_duration_exact && required_ts > max_ts {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        let is_seekable = self.reader.is_seekable();

        // If the stream is unseekable and the required timestamp in the past, then return an
        // error, it is not possible to seek to it.
        if !is_seekable && required_ts < self.next_packet_ts {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        debug!("seeking to ts={required_ts}");

        // Step 1
        //
        // MPEG audio frames have no timestamps, therefore the position of the frame containing the
        // required timestamp can only be exactly known by counting frames from a known position.
        // Both the coarse and accurate seek modes use the same procedure since a byte-position
        // based estimate of the timestamp of a frame cannot be exact for variable bit-rate streams.
        //
        // Start from the closest known position before the target, but leave enough frames before
        // the target for the bit reservoir and overlap-add pre-roll. If the current position is
        // already closer than any known position, then continue from it.
        let scan_limit_ts =
            required_ts.saturating_sub(self.frame_dur.saturating_mul(SEEK_SCAN_MARGIN_FRAMES));

        let is_continuing = self.next_packet_ts <= scan_limit_ts;

        if is_seekable {
            let start = self.seek_index.lookup(scan_limit_ts);

            if !is_continuing || start.ts > self.next_packet_ts {
                debug!("seeking to index entry ts={} @ pos={}", start.ts, start.pos);

                let pos = self.reader.seek(SeekFrom::Start(start.pos))?;

                // Since the elementary stream has no timestamp information, the position seeked
                // to must be exactly as requested.
                if pos != start.pos {
                    return seek_error(SeekErrorKind::Unseekable);
                }

                self.next_packet_ts = start.ts;
            }
        }

        // Step 2
        //
        // Parse MPEG frames one-by-one from the current position in the stream until the frame
        // containing the desired timestamp is reached. Keep a history of the most recent frames.
        let mut history = VecDeque::with_capacity(SEEK_HISTORY_FRAMES);

        let mut ts = self.next_packet_ts;

        loop {
            let frame = match scan_frame(&mut self.reader, self.free_len, self.data_end)? {
                Some(frame) => frame,
                // MPEG streams have no well-defined end, so if no more frames can be read then
                // assume the seek position is out-of-range. This would normally only happen if
                // the duration of the track is unknown, or it is was longer than the track
                // actually is.
                None => return seek_error(SeekErrorKind::OutOfRange),
            };

            self.seek_index.insert(ts, frame.pos);

            if history.len() == SEEK_HISTORY_FRAMES {
                history.pop_front();
            }

            history.push_back(HistoryFrame {
                ts,
                pos: frame.pos,
                main_data_begin: frame.main_data_begin,
                main_data_len: frame.main_data_len,
            });

            match ts.checked_add(frame.header.duration()) {
                Some(next_ts) if next_ts <= required_ts => ts = next_ts,
                // The timestamp of the next MPEG frame exceeds the desired timestamp, or it
                // exceeds the representable range. The current frame is the target frame.
                _ => break,
            }
        }

        // Step 3
        //
        // To decode the target frame exactly as a continuous decode would, frames before it must be
        // provided to the decoder as well. Find the oldest one required, and seek to it.
        let start_idx = find_preroll_start(&history);
        let mut start = history[start_idx];

        debug!(
            "found frame with ts={} @ pos={}, will seek -{} frame(s) to ts={} @ pos={}",
            ts,
            history[history.len() - 1].pos,
            history.len() - 1 - start_idx,
            start.ts,
            start.pos
        );

        // Seek to the start frame. All the frames since are likely still buffered.
        if self.reader.seek_buffered(start.pos) != start.pos {
            if is_seekable {
                self.reader.seek(SeekFrom::Start(start.pos))?;
            }
            else {
                // The stream cannot be seeked to the oldest frame needed, use the oldest frame
                // that is buffered.
                let seeked_pos = self.reader.pos();

                start = *history
                    .iter()
                    .find(|frame| frame.pos >= seeked_pos)
                    .expect("target frame is always buffered");

                self.reader.seek_buffered(start.pos);
            }
        }

        self.next_packet_ts = start.ts;

        debug!(
            "seeked to ts={} (delta={})",
            self.next_packet_ts,
            self.next_packet_ts.saturating_delta(required_ts),
        );

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: self.next_packet_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

impl<'s> MpaReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // An MPEG program stream contains MPEG audio frames, but it is not an MPEG audio stream.
        if is_mpeg_ps_stream(&mut mss) {
            return unsupported_error("mp3: mpeg program streams are not supported");
        }

        // Free-format streams have no bit-rate in their frame headers, so the length of their frames
        // must be found first.
        let free_len = detect_free_format_len(&mut mss);

        // Try to read the first MPEG frame.
        let (header, packet) = read_mpeg_frame_strict(&mut mss, free_len)?;

        // Use the header to populate the codec parameters.
        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(header.codec())
            .with_sample_rate(header.sample_rate)
            .with_channels(header.channel_mode.channels());

        // Create the track.
        let mut track = Track::new(0);

        track.with_codec_params(CodecParameters::Audio(codec_params));

        // Whether the duration is exact (from a tag or scanning), or estimated.
        let mut is_duration_exact = false;

        // Check if there is a Xing/Info tag contained in the first frame.
        if let Some(info_tag) = try_read_info_tag(&packet, &header) {
            // The LAME tag contains ReplayGain and padding information.
            if let Some(lame_tag) = info_tag.lame {
                track.with_delay(lame_tag.enc_delay).with_padding(lame_tag.enc_padding);
            }

            // The base Xing/Info tag may contain the number of MPEG frames.
            if let Some(num_mpeg_frames) = info_tag.num_frames {
                info!("using xing header for duration");

                // The total number of audio frames including delay and padding frames.
                let num_frames = u64::from(num_mpeg_frames) * u64::from(header.num_frames());

                // Remove delay and padding frames.
                let discard = track.delay.unwrap_or(0) + track.padding.unwrap_or(0);

                track.with_num_frames(num_frames.saturating_sub(u64::from(discard)));
                is_duration_exact = true;
            }
        }
        else if let Some(vbri_tag) = try_read_vbri_tag(&packet, &header) {
            info!("using vbri header for duration");

            let num_frames = u64::from(vbri_tag.num_mpeg_frames) * u64::from(header.num_frames());

            // Check if there is a VBRI tag.
            track.with_num_frames(num_frames);
            is_duration_exact = true;
        }
        else {
            // The first frame was not a Xing/Info header, rewind back to the start of the frame so
            // that it may be decoded.
            let frame_start = mss.pos() - (MPEG_HEADER_LEN + header.frame_size) as u64;
            rewind_to(&mut mss, frame_start)?;
        }

        // The first frame of audio data.
        let first_packet_pos = mss.pos();

        // Find where the audio data ends, excluding any tags appended after it.
        let mut data_end = match mss.byte_len() {
            Some(len) if mss.is_seekable() => {
                Some(trailer::find_audio_end(&mut mss, first_packet_pos, len))
            }
            _ => None,
        };

        let mut metadata = opts.external_data.metadata.unwrap_or_default();
        let mut chapters = opts.external_data.chapters;

        // An ID3v2.4 tag may be appended to the audio data. It is the only kind of ID3v2 tag that
        // can be, and is not found by the probe since it is not at the start of the stream.
        if let Some(len) = mss.byte_len().filter(|_| mss.is_seekable()) {
            if let Ok(Some(tag)) = id3v2::read_trailing_id3v2(&mut mss, len) {
                debug!("found a trailing id3v2 tag @ {}..{}", tag.start, tag.end);

                metadata.push(tag.metadata.revision);

                for side_data in tag.metadata.side_data {
                    match side_data {
                        MetadataSideData::Chapters(group) if chapters.is_none() => {
                            chapters = Some(group);
                        }
                        _ => (),
                    }
                }

                // The audio data ends where the tag begins, if it didn't already.
                data_end = data_end.map(|end| end.min(tag.start));
            }
        }

        // The first seek point is the first frame.
        let min_ts = Timestamp::from(-i64::from(track.delay.unwrap_or(0)));
        let frame_dur = header.duration();

        // The seek index entries are at least `seek_index_fill_period_ms` apart, and at least 1
        // frame apart.
        let period = Duration::from(
            u64::from(opts.seek_index_fill_period_ms) * u64::from(header.sample_rate) / 1000,
        )
        .max(frame_dur);

        let mut seek_index =
            SeekIndex::new(period, SeekPoint { ts: min_ts, pos: first_packet_pos });

        // If the stream is seekable, and the number of frames is not known, then either scan the
        // entire stream to count them (and build the seek index), or estimate them if scanning
        // wasn't requested.
        if mss.is_seekable() && track.num_frames.is_none() {
            let audio_end = data_end.or(mss.byte_len());

            if opts.prebuild_seek_index {
                info!("scanning stream for duration");

                if let Some(n_mpeg_frames) =
                    scan_num_mpeg_frames(&mut mss, free_len, data_end, &mut seek_index, min_ts)
                {
                    track.with_num_frames(n_mpeg_frames * u64::from(header.num_frames()));
                    is_duration_exact = true;
                }
            }
            else if let Some(audio_end) = audio_end {
                info!("estimating duration from bitrate, may be inaccurate for vbr files");

                if let Some(n_mpeg_frames) = estimate_num_mpeg_frames(
                    &mut mss,
                    &header,
                    free_len,
                    first_packet_pos,
                    audio_end,
                ) {
                    track.with_num_frames(n_mpeg_frames * u64::from(header.num_frames()));
                }
            }
        }
        else if mss.is_seekable() && opts.prebuild_seek_index {
            // Build the seek index only. The duration is already known.
            let _ = scan_num_mpeg_frames(&mut mss, free_len, data_end, &mut seek_index, min_ts);
        }

        if let Some(num_frames) = track.num_frames {
            // Duration equals the number of frames because the timebase is always 1 / sample rate.
            track.with_duration(Duration::from(num_frames));
        }

        let next_packet_ts = min_ts;

        Ok(MpaReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters,
            metadata,
            next_packet_ts,
            data_end,
            free_len,
            is_duration_exact,
            frame_dur,
            seek_index,
        })
    }
}

/// A frame in the history of frames kept while seeking.
#[derive(Copy, Clone)]
struct HistoryFrame {
    ts: Timestamp,
    pos: u64,
    main_data_begin: u16,
    main_data_len: usize,
}

/// Given the history of frames up-to and including the frame containing the seek target (the last
/// frame in the history), find the index of the oldest frame that must be decoded.
///
/// The output of the target frame depends on the main data of the target frame and of
/// `SEEK_PREROLL_FRAMES` frames before it. The main data of a frame may begin in an earlier frame,
/// `main_data_begin` bytes (of main data only, side information and headers are not counted) before
/// the end of the main data of the previous frame.
fn find_preroll_start(history: &VecDeque<HistoryFrame>) -> usize {
    let target = history.len() - 1;
    let first = target.saturating_sub(SEEK_PREROLL_FRAMES);

    let mut start = first;

    for frame_idx in first..=target {
        let mut remaining = usize::from(history[frame_idx].main_data_begin);
        let mut begin_idx = frame_idx;

        // Walk back through the main data of the previous frames until the main data of the frame
        // is covered.
        while remaining > 0 && begin_idx > 0 {
            begin_idx -= 1;
            remaining = remaining.saturating_sub(history[begin_idx].main_data_len);
        }

        start = start.min(begin_idx);
    }

    start
}

/// Seek `reader` to an absolute position. The position is most likely within the buffer.
fn rewind_to(reader: &mut MediaSourceStream<'_>, pos: u64) -> Result<()> {
    if reader.seek_buffered(pos) != pos {
        reader.seek(SeekFrom::Start(pos))?;
    }

    Ok(())
}

/// Returns true if the frame header word is a (otherwise valid) free-format header.
fn is_free_format_word(word: u32) -> bool {
    header::is_frame_header_word_synced(word)
        && header::check_header(word)
        && (word >> 12) & 0xf == 0
}

/// Read the first bytes of a stream that begins with a free-format frame header, `first`, that has
/// already been read, enough to find the length of the free-format frames.
fn read_free_format_window<B: ReadBytes>(reader: &mut B, first: u32) -> Vec<u8> {
    let mut buf = vec![0u8; MPEG_HEADER_LEN + 3 * (MAX_MPEG_FRAME_SIZE + 1)];
    buf[..MPEG_HEADER_LEN].copy_from_slice(&first.to_be_bytes());

    let mut len = MPEG_HEADER_LEN;

    while len < buf.len() {
        match reader.read_buf(&mut buf[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
    }

    buf.truncate(len);
    buf
}

/// Check if the stream begins with free-format frames and, if so, return the length of the frames
/// (including the header, excluding padding). The position of the stream is not changed.
fn detect_free_format_len(reader: &mut MediaSourceStream<'_>) -> Option<usize> {
    /// The maximum number of bytes of junk before the first frame that are skipped to find it.
    const MAX_JUNK_LEN: u64 = 4096;

    let start = reader.pos();
    let mut free_len = None;

    while let Ok(word) = header::sync_frame(reader) {
        // Give up if the first frame is too far into the stream.
        if reader.pos() - start > MAX_JUNK_LEN + MPEG_HEADER_LEN as u64 {
            break;
        }

        if is_free_format_word(word) {
            let window = read_free_format_window(reader, word);

            // Move the position to the byte after the header again.
            let end = reader.pos();
            let _ = rewind_to(reader, end - (window.len() - MPEG_HEADER_LEN) as u64);

            if let Some(len) = header::find_free_format_len(&window) {
                free_len = Some(len);
                break;
            }
        }
        else if header::parse_frame_header(word).is_ok() {
            // The first frame is a regular frame.
            break;
        }
    }

    // Restore the original position.
    let _ = rewind_to(reader, start);

    free_len
}

/// Reads a MPEG frame and returns the header and buffer.
fn read_mpeg_frame(
    reader: &mut MediaSourceStream<'_>,
    free_len: Option<usize>,
) -> Result<(FrameHeader, Vec<u8>)> {
    let (header, header_word) = loop {
        // Sync to the next frame header.
        let sync = header::sync_frame(reader)?;

        // Parse the frame header fully.
        if let Ok(header) = header::parse_frame_header_free(sync, free_len) {
            break (header, sync);
        }

        warn!("invalid mpeg audio header");
    };

    // Allocate frame buffer.
    let mut packet = vec![0u8; MPEG_HEADER_LEN + header.frame_size];
    packet[0..MPEG_HEADER_LEN].copy_from_slice(&header_word.to_be_bytes());

    // Read the frame body.
    reader.read_buf_exact(&mut packet[MPEG_HEADER_LEN..])?;

    // Return the parsed header and packet body.
    Ok((header, packet))
}

/// Reads a MPEG frame and checks if the next frame begins after the packet.
fn read_mpeg_frame_strict(
    reader: &mut MediaSourceStream<'_>,
    free_len: Option<usize>,
) -> Result<(FrameHeader, Vec<u8>)> {
    loop {
        // Read the next MPEG frame.
        let (header, packet) = read_mpeg_frame(reader, free_len)?;

        // Get the position before trying to read the next header.
        let pos = reader.pos();

        // Read a sync word from the stream. If this read fails then the file may have ended and
        // this check cannot be performed.
        if let Ok(sync) = header::read_frame_header_word_no_sync(reader) {
            // If the stream is not synced to the next frame's sync word, or the next frame header
            // is not parseable or similar to the current frame header, then reject the current
            // packet since the stream likely synced to random data.
            if !header::is_frame_header_word_synced(sync)
                || !is_frame_header_similar(&header, sync, free_len)
            {
                warn!("skipping junk at {} bytes", pos - packet.len() as u64);

                // Seek back to the second byte of the rejected packet to prevent syncing to the
                // same spot again.
                reader.seek_buffered_rev(packet.len() + MPEG_HEADER_LEN - 1);
                continue;
            }
        }

        // Jump back to the position before the next header was read.
        reader.seek_buffered(pos);

        break Ok((header, packet));
    }
}

/// Check if a sync word parses to a frame header that is similar to the one provided.
fn is_frame_header_similar(header: &FrameHeader, sync: u32, free_len: Option<usize>) -> bool {
    if let Ok(candidate) = header::parse_frame_header_free(sync, free_len) {
        if header.version == candidate.version
            && header.layer == candidate.layer
            && header.sample_rate == candidate.sample_rate
            && header.n_channels() == candidate.n_channels()
        {
            return true;
        }
    }

    false
}

/// Reads the main_data_begin field from the side information of a MPEG audio frame.
fn read_main_data_begin<B: ReadBytes>(reader: &mut B, header: &FrameHeader) -> Result<u16> {
    // After the head the optional CRC is present.
    if header.has_crc {
        let _crc = reader.read_be_u16()?;
    }

    // For MPEG version 1 the first 9 bits is main_data_begin.
    let main_data_begin = if header.is_mpeg1() {
        reader.read_be_u16()? >> 7
    }
    // For MPEG version 2 the first 8 bits is main_data_begin.
    else {
        u16::from(reader.read_u8()?)
    };

    Ok(main_data_begin)
}

/// Skip to the next MPEG frame, reading as little of it as possible. Frames that `next_packet`
/// discards (Xing, Info, and VBRI tags) are skipped, as are frames that extend beyond the end of
/// the audio data.
///
/// Returns `Ok(None)` if there are no more frames.
fn scan_frame(
    reader: &mut MediaSourceStream<'_>,
    free_len: Option<usize>,
    data_end: Option<u64>,
) -> Result<Option<ScannedFrame>> {
    // The end of the audio data is the end of the stream, if it is known and nothing is earlier.
    let end = match (data_end, reader.byte_len()) {
        (Some(data_end), Some(len)) => Some(data_end.min(len)),
        (end, len) => end.or(len),
    };

    loop {
        // Sync to the next frame.
        let sync = match header::sync_frame(reader) {
            Ok(sync) => sync,
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        // Parse the synced frame header. Ignore invalid headers.
        let header = match header::parse_frame_header_free(sync, free_len) {
            Ok(header) => header,
            Err(_) => {
                warn!("invalid mpeg audio header");
                continue;
            }
        };

        // Position of the frame header.
        let pos = reader.pos() - MPEG_HEADER_LEN as u64;

        // The frame must be entirely within the audio data.
        if end.is_some_and(|end| pos + (MPEG_HEADER_LEN + header.frame_size) as u64 > end) {
            return Ok(None);
        }

        // Only Layer 3 has a bit reservoir and may contain tags.
        if header.layer != MpegLayer::Layer3 {
            reader.ignore_bytes(header.frame_size as u64)?;

            return Ok(Some(ScannedFrame { pos, header, main_data_begin: 0, main_data_len: 0 }));
        }

        // Read the start of the frame. It contains the side information, and the Xing/Info/VBRI
        // tag, if present.
        let prefix_len = header.frame_size.min(SCAN_PREFIX_LEN);

        let mut prefix = [0u8; MPEG_HEADER_LEN + SCAN_PREFIX_LEN];
        prefix[..MPEG_HEADER_LEN].copy_from_slice(&sync.to_be_bytes());

        let buf = &mut prefix[..MPEG_HEADER_LEN + prefix_len];
        reader.read_buf_exact(&mut buf[MPEG_HEADER_LEN..])?;

        let is_info = is_maybe_info_tag(buf, &header);

        let main_data_begin = if is_info || is_maybe_vbri_tag(buf, &header) {
            // Might be a tag. A full check requires the whole frame.
            let mut frame = vec![0u8; MPEG_HEADER_LEN + header.frame_size];
            frame[..buf.len()].copy_from_slice(buf);
            reader.read_buf_exact(&mut frame[buf.len()..])?;

            let is_tag = if is_info {
                try_read_info_tag(&frame, &header).is_some()
            }
            else {
                try_read_vbri_tag(&frame, &header).is_some()
            };

            if is_tag {
                continue;
            }

            read_main_data_begin(&mut BufReader::new(&buf[MPEG_HEADER_LEN..]), &header)
        }
        else {
            // Ignore the rest of the frame.
            reader.ignore_bytes((header.frame_size - prefix_len) as u64)?;

            read_main_data_begin(&mut BufReader::new(&buf[MPEG_HEADER_LEN..]), &header)
        }
        .unwrap_or(0);

        let main_data_len = header
            .frame_size
            .saturating_sub(if header.has_crc { 2 } else { 0 } + header.side_info_len());

        return Ok(Some(ScannedFrame { pos, header, main_data_begin, main_data_len }));
    }
}

/// Counts the MPEG frames in the stream, starting from the first frame at the current position.
/// Adds entries to the seek index. The stream is seeked back to the first frame.
fn scan_num_mpeg_frames(
    reader: &mut MediaSourceStream<'_>,
    free_len: Option<usize>,
    data_end: Option<u64>,
    seek_index: &mut SeekIndex,
    first_ts: Timestamp,
) -> Option<u64> {
    let start_pos = reader.pos();

    let mut ts = first_ts;
    let mut n_frames = 0u64;

    let is_complete = loop {
        match scan_frame(reader, free_len, data_end) {
            Ok(Some(frame)) => {
                seek_index.insert(ts, frame.pos);
                n_frames += 1;

                match ts.checked_add(frame.header.duration()) {
                    Some(next_ts) => ts = next_ts,
                    None => break false,
                }
            }
            Ok(None) => break true,
            Err(_) => break false,
        }
    };

    // Rewind back to the first frame.
    rewind_to(reader, start_pos).ok()?;

    if is_complete { Some(n_frames) } else { None }
}

/// Estimates the total number of MPEG frames in the audio data of the media source stream, which
/// spans `start_pos..end_pos`, by sampling some frames throughout it.
///
/// The position of the stream is restored.
fn estimate_num_mpeg_frames(
    reader: &mut MediaSourceStream<'_>,
    first_header: &FrameHeader,
    free_len: Option<usize>,
    start_pos: u64,
    end_pos: u64,
) -> Option<u64> {
    let total_len = end_pos.checked_sub(start_pos)?;

    let mut total_frame_len = 0u64;
    let mut total_frames = 0u64;
    let mut is_constant_bitrate = true;

    // Sample windows of frames evenly spaced throughout the audio data. The average length of the
    // frames sampled is used to calculate the total number of frames. For constant bit-rate streams
    // all windows have the same frame length, but for variable bit-rate streams, the windows are
    // much more representative than just the start of the stream.
    for window in 0..ESTIMATE_WINDOWS {
        // The middle of the window's section of the stream.
        let window_pos = start_pos
            + (u128::from(total_len) * u128::from(2 * window + 1)
                / u128::from(2 * ESTIMATE_WINDOWS)) as u64;

        if rewind_to(reader, window_pos).is_err() {
            break;
        }

        // Find the first frame of the window.
        let first = read_mpeg_frame_strict(reader, free_len).ok();

        let Some((header, packet)) = first
        else {
            continue;
        };

        let mut frame_header = header;
        let mut frame_len = packet.len();
        let mut n_window_frames = 0;

        loop {
            if frame_header.bitrate != first_header.bitrate {
                is_constant_bitrate = false;
            }

            total_frame_len += frame_len as u64;
            total_frames += 1;
            n_window_frames += 1;

            if n_window_frames == ESTIMATE_WINDOW_FRAMES {
                break;
            }

            // Read the next consecutive frame header in the window.
            let next = reader
                .read_be_u32()
                .ok()
                .and_then(|word| header::parse_frame_header_free(word, free_len).ok());

            let Some(next) = next
            else {
                break;
            };

            if reader.ignore_bytes(next.frame_size as u64).is_err() {
                break;
            }

            frame_len = MPEG_HEADER_LEN + next.frame_size;
            frame_header = next;
        }
    }

    // Restore the original position.
    rewind_to(reader, start_pos).ok()?;

    if total_frames == 0 {
        return None;
    }

    let mut avg_frame_len = total_frame_len as f64 / total_frames as f64;

    // For a constant bit-rate stream, the average frame length is exactly the length implied by
    // the bit-rate (the length without padding plus the fraction of a slot, depending on how often
    // padding is used). The sampled frames may over- or under-represent padding.
    if is_constant_bitrate && free_len.is_none() {
        avg_frame_len = f64::from(first_header.num_frames()) / 8.0
            * f64::from(first_header.bitrate)
            / f64::from(first_header.sample_rate);

        // A stream of whole frames has an integral number of frames, but the division by the
        // average length is inexact, and truncating it would drop the last frame. A fraction of a
        // frame this close to the next frame boundary is rounding error, not a partial frame.
        return Some((total_len as f64 / avg_frame_len + 0.05) as u64);
    }

    Some((total_len as f64 / avg_frame_len) as u64)
}

const XING_TAG_ID: [u8; 4] = *b"Xing";
const INFO_TAG_ID: [u8; 4] = *b"Info";

/// The LAME tag is an extension to the Xing/Info tag.
#[allow(dead_code)]
struct LameTag {
    encoder: String,
    replaygain_peak: Option<f32>,
    replaygain_radio: Option<f32>,
    replaygain_audiophile: Option<f32>,
    enc_delay: u32,
    enc_padding: u32,
}

/// The Xing/Info time additional information for regarding a MP3 file.
#[allow(dead_code)]
struct XingInfoTag {
    num_frames: Option<u32>,
    num_bytes: Option<u32>,
    toc: Option<[u8; 100]>,
    quality: Option<u32>,
    is_cbr: bool,
    lame: Option<LameTag>,
}

/// Try to read a Xing/Info tag from the provided MPEG frame.
fn try_read_info_tag(buf: &[u8], header: &FrameHeader) -> Option<XingInfoTag> {
    // The Info header is a completely optional piece of information. Therefore, flatten an error
    // reading the tag into a None.
    try_read_info_tag_inner(buf, header).ok().flatten()
}

fn try_read_info_tag_inner(buf: &[u8], header: &FrameHeader) -> Result<Option<XingInfoTag>> {
    // Do a quick check that this is a Xing/Info tag.
    if !is_maybe_info_tag(buf, header) {
        return Ok(None);
    }

    // The position of the Xing/Info tag relative to the end of the header. This is equal to the
    // side information length for the frame. The CRC is not included in this offset calculation.
    let offset = MPEG_HEADER_LEN + header.side_info_len();

    // Start the CRC with the header and side information.
    let mut crc16 = Crc16AnsiLe::new(0);
    crc16.process_buf_bytes(&buf[..offset]);

    // Start reading the Xing/Info tag after the side information.
    let mut reader = MonitorStream::new(BufReader::new(&buf[offset..]), crc16);

    // Check for Xing/Info header.
    let id = reader.read_quad_bytes()?;

    if id != XING_TAG_ID && id != INFO_TAG_ID {
        return Ok(None);
    }

    // The "Info" id is used for CBR files.
    let is_cbr = id == INFO_TAG_ID;

    // Flags indicates what information is provided in this Xing/Info tag.
    let flags = reader.read_be_u32()?;

    let num_frames = if flags & 0x1 != 0 { Some(reader.read_be_u32()?) } else { None };

    let num_bytes = if flags & 0x2 != 0 { Some(reader.read_be_u32()?) } else { None };

    let toc = if flags & 0x4 != 0 {
        let mut toc = [0; 100];
        reader.read_buf_exact(&mut toc)?;
        Some(toc)
    }
    else {
        None
    };

    let quality = if flags & 0x8 != 0 { Some(reader.read_be_u32()?) } else { None };

    /// The full LAME extension size.
    const LAME_EXT_LEN: u64 = 36;
    /// The minimal LAME extension size up-to the encode delay & padding fields.
    const MIN_LAME_EXT_LEN: u64 = 24;

    // The LAME extension may not always be present, or complete. The important fields in the
    // extension are within the first 24 bytes. Therefore, try to read those if they're available.
    let lame = if reader.inner().bytes_available() >= MIN_LAME_EXT_LEN {
        // Encoder string.
        let mut encoder = [0; 9];
        reader.read_buf_exact(&mut encoder)?;

        // Revision.
        let _revision = reader.read_u8()?;

        // Lowpass filter value.
        let _lowpass = reader.read_u8()?;

        // Replay gain peak in 9.23 (bit) fixed-point format.
        let replaygain_peak = match reader.read_be_u32()? {
            0 => None,
            peak => Some(32767.0 * (peak as f32 / 2.0f32.powi(23))),
        };

        // Radio replay gain.
        let replaygain_radio = parse_lame_tag_replaygain(reader.read_be_u16()?, 1);

        // Audiophile replay gain.
        let replaygain_audiophile = parse_lame_tag_replaygain(reader.read_be_u16()?, 2);

        // Encoding flags & ATH type.
        let _encoding_flags = reader.read_u8()?;

        // Arbitrary bitrate.
        let _abr = reader.read_u8()?;

        let (enc_delay, enc_padding) = {
            let trim = reader.read_be_u24()?;

            if encoder[..4] == *b"LAME" || encoder[..4] == *b"Lavf" || encoder[..4] == *b"Lavc" {
                let delay = 528 + 1 + (trim >> 12);
                let padding = trim & ((1 << 12) - 1);

                (delay, padding.saturating_sub(528 + 1))
            }
            else {
                (0, 0)
            }
        };

        // If possible, attempt to read the extra fields of the extension if they weren't
        // truncated.
        let crc = if reader.inner().bytes_available() >= LAME_EXT_LEN - MIN_LAME_EXT_LEN {
            // Flags.
            let _misc = reader.read_u8()?;

            // MP3 gain.
            let _mp3_gain = reader.read_u8()?;

            // Preset and surround info.
            let _surround_info = reader.read_be_u16()?;

            // Music length.
            let _music_len = reader.read_be_u32()?;

            // Music (audio) CRC.
            let _music_crc = reader.read_be_u16()?;

            // The tag CRC. LAME always includes this CRC regardless of the protection bit, but
            // other encoders may only do so if the protection bit is set.
            if header.has_crc || encoder[..4] == *b"LAME" {
                // Read the CRC using the inner reader to not change the computed CRC.
                Some(reader.inner_mut().read_be_u16()?)
            }
            else {
                // No CRC is present.
                None
            }
        }
        else {
            // The tag is truncated. No CRC will be present.
            info!("xing tag lame extension is truncated");
            None
        };

        // If there was no CRC written, then assume the tag is correct. Otherwise, use the CRC.
        // Accept a written CRC of 0 which defacto means to ignore the CRC.
        let is_tag_ok = crc.is_none_or(|crc| crc == 0 || crc == reader.monitor().crc());

        if is_tag_ok {
            // The CRC matched or is not present.
            Some(LameTag {
                encoder: String::from_utf8_lossy(&encoder).into(),
                replaygain_peak,
                replaygain_radio,
                replaygain_audiophile,
                enc_delay,
                enc_padding,
            })
        }
        else {
            // The CRC did not match, this is probably not a LAME tag.
            warn!("xing tag lame extension crc mismatch");
            None
        }
    }
    else {
        // Frame not large enough for a LAME tag.
        info!("xing tag too small for lame extension");
        None
    };

    Ok(Some(XingInfoTag { num_frames, num_bytes, toc, quality, is_cbr, lame }))
}

fn parse_lame_tag_replaygain(value: u16, expected_name: u8) -> Option<f32> {
    // The 3 most-significant bits are the name code.
    let name = ((value & 0xe000) >> 13) as u8;

    if name == expected_name {
        let gain = (value & 0x01ff) as f32 / 10.0;
        Some(if value & 0x200 != 0 { -gain } else { gain })
    }
    else {
        None
    }
}

/// Perform a fast check to see if the packet contains a Xing/Info tag. If this returns true, the
/// packet should be parsed fully to ensure it is in fact a tag.
fn is_maybe_info_tag(buf: &[u8], header: &FrameHeader) -> bool {
    const MIN_XING_TAG_LEN: usize = 8;

    // Only supported with layer 3 packets.
    if header.layer != MpegLayer::Layer3 {
        return false;
    }

    // The position of the Xing/Info tag relative to the start of the packet. This is equal to the
    // side information length for the frame. The CRC is not included in this offset calculation.
    let offset = MPEG_HEADER_LEN + header.side_info_len();

    // The packet must be big enough to contain a tag.
    if buf.len() < offset + MIN_XING_TAG_LEN {
        return false;
    }

    // The tag ID must be present and correct.
    let id = &buf[offset..offset + 4];

    if id != XING_TAG_ID && id != INFO_TAG_ID {
        return false;
    }

    // The side information, which follows the header and optional CRC, should be zeroed.
    !buf[header.header_size()..offset].iter().any(|&b| b != 0)
}

const VBRI_TAG_ID: [u8; 4] = *b"VBRI";

/// The contents of a VBRI tag.
#[allow(dead_code)]
struct VbriTag {
    num_bytes: u32,
    num_mpeg_frames: u32,
}

/// Try to read a VBRI tag from the provided MPEG frame.
fn try_read_vbri_tag(buf: &[u8], header: &FrameHeader) -> Option<VbriTag> {
    // The VBRI header is a completely optional piece of information. Therefore, flatten an error
    // reading the tag into a None.
    try_read_vbri_tag_inner(buf, header).ok().flatten()
}

fn try_read_vbri_tag_inner(buf: &[u8], header: &FrameHeader) -> Result<Option<VbriTag>> {
    // Do a quick check that this is a VBRI tag.
    if !is_maybe_vbri_tag(buf, header) {
        return Ok(None);
    }

    let mut reader = BufReader::new(buf);

    // The VBRI tag is always 32 bytes after the header.
    reader.ignore_bytes(MPEG_HEADER_LEN as u64 + 32)?;

    // Check for the VBRI signature.
    let id = reader.read_quad_bytes()?;

    if id != VBRI_TAG_ID {
        return Ok(None);
    }

    // The version is always 1.
    let version = reader.read_be_u16()?;

    if version != 1 {
        return Ok(None);
    }

    // Delay is a 2-byte big-endiann floating point value?
    let _delay = reader.read_be_u16()?;
    let _quality = reader.read_be_u16()?;

    let num_bytes = reader.read_be_u32()?;
    let num_mpeg_frames = reader.read_be_u32()?;

    Ok(Some(VbriTag { num_bytes, num_mpeg_frames }))
}

/// Perform a fast check to see if the packet contains a VBRI tag. If this returns true, the
/// packet should be parsed fully to ensure it is in fact a tag.
fn is_maybe_vbri_tag(buf: &[u8], header: &FrameHeader) -> bool {
    const MIN_VBRI_TAG_LEN: usize = 26;
    const VBRI_TAG_OFFSET: usize = MPEG_HEADER_LEN + 32;

    // Only supported with layer 3 packets.
    if header.layer != MpegLayer::Layer3 {
        return false;
    }

    // The packet must be big enough to contain a tag.
    if buf.len() < VBRI_TAG_OFFSET + MIN_VBRI_TAG_LEN {
        return false;
    }

    // The tag ID must be present and correct.
    let id = &buf[VBRI_TAG_OFFSET..VBRI_TAG_OFFSET + 4];

    if id != VBRI_TAG_ID {
        return false;
    }

    // The bytes preceeding the VBRI tag (mostly the side information) should be all 0.
    !buf[header.header_size()..VBRI_TAG_OFFSET].iter().any(|&b| b != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{ChannelMode, Emphasis, MpegVersion};

    #[test]
    fn test_is_maybe_info_tag_with_crc() {
        let header = FrameHeader {
            version: MpegVersion::Mpeg1,
            layer: MpegLayer::Layer3,
            bitrate: 128000,
            sample_rate: 44100,
            sample_rate_idx: 0,
            channel_mode: ChannelMode::Stereo,
            emphasis: Emphasis::None,
            is_copyrighted: false,
            is_original: false,
            has_padding: false,
            has_crc: true,
            frame_size: 417,
        };

        let mut buf = vec![0u8; 100];
        // Header
        buf[0..4].copy_from_slice(&[0xff, 0xfa, 0x90, 0x44]);
        // CRC (non-zero)
        buf[4] = 0x12;
        buf[5] = 0x34;
        // Tag ID at offset 32 + 4 = 36
        buf[36..40].copy_from_slice(b"Xing");

        // Ensure the heuristic returns true even if a non-zero CRC is present.
        assert!(is_maybe_info_tag(&buf, &header));
    }

    #[test]
    fn test_is_maybe_vbri_tag_with_crc() {
        let header = FrameHeader {
            version: MpegVersion::Mpeg1,
            layer: MpegLayer::Layer3,
            bitrate: 128000,
            sample_rate: 44100,
            sample_rate_idx: 0,
            channel_mode: ChannelMode::Stereo,
            emphasis: Emphasis::None,
            is_copyrighted: false,
            is_original: false,
            has_padding: false,
            has_crc: true,
            frame_size: 417,
        };

        let mut buf = vec![0u8; 100];
        // Header
        buf[0..4].copy_from_slice(&[0xff, 0xfa, 0x90, 0x44]);
        // CRC (non-zero)
        buf[4] = 0x12;
        buf[5] = 0x34;
        // Tag ID at offset 36
        buf[36..40].copy_from_slice(b"VBRI");

        // Ensure the heuristic returns true even if a non-zero CRC is present.
        assert!(is_maybe_vbri_tag(&buf, &header));
    }

    // Synthetic streams of MPEG1 Layer 3, 44.1 kHz, stereo frames. The frames contain no audio.
    // The index of each frame is stored in its last 4 bytes, and its main_data_begin in its side
    // information.

    use std::io::Cursor;

    /// The length of the frames of the synthetic streams excluding padding, including the header.
    const FRAME_LEN: usize = 417;

    struct SyntheticFrame {
        /// The main_data_begin value.
        main_data_begin: u16,
        padding: bool,
    }

    /// Build a stream of frames. If `bitrate_idx` is 0, the stream is free-format with the
    /// frame length `frame_len`.
    fn build_stream(frames: &[SyntheticFrame], bitrate_idx: u8, frame_len: usize) -> Vec<u8> {
        let mut data = Vec::new();

        for (idx, frame) in frames.iter().enumerate() {
            let len = frame_len + usize::from(frame.padding);
            let start = data.len();

            // MPEG1, Layer 3, no CRC, 44.1 kHz, stereo.
            data.extend_from_slice(&[
                0xff,
                0xfb,
                (bitrate_idx << 4) | (u8::from(frame.padding) << 1),
                0x00,
            ]);
            data.resize(start + len, 0);

            // The first 9 bits of the side information are main_data_begin.
            let mdb = (frame.main_data_begin << 7).to_be_bytes();
            data[start + 4..start + 6].copy_from_slice(&mdb);

            data[start + len - 4..start + len].copy_from_slice(&(idx as u32).to_be_bytes());
        }

        data
    }

    fn plain_frames(n: usize) -> Vec<SyntheticFrame> {
        // 128 kbps @ 44.1 kHz needs padding in 24 of 25 frames.
        (0..n).map(|i| SyntheticFrame { main_data_begin: 0, padding: i % 25 != 0 }).collect()
    }

    fn open_stream(data: Vec<u8>, seekable: bool, opts: FormatOptions) -> MpaReader<'static> {
        let mss = if seekable {
            MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
        }
        else {
            MediaSourceStream::new(
                Box::new(ReadOnlySource::new(Cursor::new(data))),
                Default::default(),
            )
        };

        MpaReader::try_new(mss, opts).unwrap()
    }

    #[test]
    fn verify_cbr_duration_estimate_counts_the_last_frame() {
        // Streams without a Xing/Info tag. The estimate must be the number of frames, not 1 less.
        for n in [10usize, 100, 231, 232, 500] {
            let data = build_stream(&plain_frames(n), 9, FRAME_LEN);
            let reader = open_stream(data, true, FormatOptions::default());

            assert_eq!(reader.tracks[0].num_frames, Some(n as u64 * 1152), "{n} frames");
        }
    }

    #[test]
    fn verify_mpeg_ps_is_rejected() {
        // An MPEG-2 program stream pack header, then a PES packet header for the audio stream
        // directly before the MPEG audio frames.
        let mut ps = vec![0x00, 0x00, 0x01, 0xba, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89];
        ps.extend_from_slice(&[0xc3, 0xf8, 0x00, 0x00, 0x01, 0xc0, 0x0f, 0xff, 0x80, 0x00, 0x00]);
        let hdr_len = ps.len() as u64;
        ps.extend_from_slice(&build_stream(&plain_frames(8), 9, FRAME_LEN));

        // Position the stream on the first frame, as the probe would.
        let new_stream = |data: Vec<u8>| {
            let mut mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
            mss.ignore_bytes(hdr_len).unwrap();
            mss
        };

        let mut mss = new_stream(ps.clone());
        let score = MpaReader::score(ScopedStream::new(&mut mss, 4096)).unwrap();
        assert!(matches!(score, Score::Unsupported));
        assert!(MpaReader::try_new(new_stream(ps), FormatOptions::default()).is_err());

        // The same frames without the pack header are supported.
        let data = build_stream(&plain_frames(8), 9, FRAME_LEN);
        let mut mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        let score = MpaReader::score(ScopedStream::new(&mut mss, 4096)).unwrap();
        assert!(matches!(score, Score::Supported(_)));
    }

    fn frame_idx(packet: &Packet) -> u32 {
        let len = packet.data.len();
        u32::from_be_bytes(packet.data[len - 4..].try_into().unwrap())
    }

    fn seek_to(reader: &mut MpaReader<'_>, mode: SeekMode, ts: i64) -> SeekedTo {
        reader.seek(mode, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 }).unwrap()
    }

    #[test]
    fn verify_seek_reports_the_timestamp_of_the_frame_returned() {
        let n = 300;
        let data = build_stream(&plain_frames(n), 9, FRAME_LEN);

        for mode in [SeekMode::Coarse, SeekMode::Accurate] {
            let mut reader = open_stream(data.clone(), true, FormatOptions::default());

            for required in (0..(n as i64 * 1152)).step_by(1777).chain([0, 1151, 1152, 1153]) {
                let seeked = seek_to(&mut reader, mode, required);

                let packet = reader.next_packet().unwrap().unwrap();

                // The frame returned is the frame reported.
                assert_eq!(packet.pts.get(), seeked.actual_ts.get(), "{mode:?} {required}");
                assert_eq!(i64::from(frame_idx(&packet)) * 1152, packet.pts.get());

                // Frames without a bit reservoir only need the preroll frames.
                let expected_frame = (required / 1152 - SEEK_PREROLL_FRAMES as i64).max(0);
                assert_eq!(seeked.actual_ts.get(), expected_frame * 1152, "{mode:?} {required}");
            }
        }
    }

    #[test]
    fn verify_seek_in_unseekable_stream() {
        let n = 300;
        let data = build_stream(&plain_frames(n), 9, FRAME_LEN);
        let mut reader = open_stream(data, false, FormatOptions::default());

        // Read a few packets, then seek forward a short distance (less than the history required),
        // and a long distance.
        for _ in 0..3 {
            reader.next_packet().unwrap().unwrap();
        }

        for required in [4 * 1152 + 5, 50 * 1152, 51 * 1152 + 1, 250 * 1152 + 700] {
            let seeked = seek_to(&mut reader, SeekMode::Accurate, required);
            let packet = reader.next_packet().unwrap().unwrap();

            assert_eq!(packet.pts, seeked.actual_ts);
            assert_eq!(i64::from(frame_idx(&packet)) * 1152, packet.pts.get());
            assert!(seeked.actual_ts.get() <= required);
            assert!(required - seeked.actual_ts.get() < 6 * 1152, "{required}");
        }

        // Backwards seeks are not possible.
        assert!(matches!(
            reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(1152), track_id: 0 }
            ),
            Err(Error::SeekError(SeekErrorKind::ForwardOnly))
        ));
    }

    #[test]
    fn verify_seek_starts_at_the_frames_containing_the_bit_reservoir() {
        let n = 200;
        let mut frames = plain_frames(n);

        // The frames before the target are 417 bytes with 4 bytes of header and 32 bytes of side
        // information, which leaves 381 bytes of main data (382 if padded).
        //
        // Frame 100 (the target) begins 500 bytes earlier, which is the whole main data of
        // frame 99 and part of frame 98.
        frames[100].main_data_begin = 500;

        // Frame 97, one of the pre-roll frames, begins 400 bytes earlier: all the main data of
        // frame 96, and the end of the main data of frame 95.
        frames[97].main_data_begin = 400;

        let data = build_stream(&frames, 9, FRAME_LEN);
        let mut reader = open_stream(data, true, FormatOptions::default());

        let seeked = seek_to(&mut reader, SeekMode::Accurate, 100 * 1152 + 3);

        assert_eq!(seeked.actual_ts.get(), 95 * 1152);

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(frame_idx(&packet), 95);
    }

    #[test]
    fn verify_find_preroll_start() {
        let frame = |main_data_begin: u16| HistoryFrame {
            ts: Timestamp::new(0),
            pos: 0,
            main_data_begin,
            main_data_len: 100,
        };

        // No reservoir: only the pre-roll frames.
        let history: VecDeque<_> = (0..10).map(|_| frame(0)).collect();
        assert_eq!(find_preroll_start(&history), 9 - SEEK_PREROLL_FRAMES);

        // Target uses 250 bytes: 3 frames of 100 bytes.
        let mut history = history;
        history[9] = frame(250);
        assert_eq!(find_preroll_start(&history), 9 - SEEK_PREROLL_FRAMES);

        // Target uses 450 bytes: 5 frames.
        history[9] = frame(450);
        assert_eq!(find_preroll_start(&history), 9 - 5);

        // A pre-roll frame uses a lot: 7 - 5 frames.
        history[9] = frame(0);
        history[7] = frame(500);
        assert_eq!(find_preroll_start(&history), 2);

        // The history is limited.
        history[7] = frame(511);
        history[6] = frame(511);
        history[5] = frame(511);
        assert_eq!(find_preroll_start(&history), 0);

        // A history of 1 frame.
        let history: VecDeque<_> = [frame(100)].into_iter().collect();
        assert_eq!(find_preroll_start(&history), 0);
    }

    #[test]
    fn verify_trailing_tags_are_not_audio() {
        let n = 100;
        let mut data = build_stream(&plain_frames(n), 9, FRAME_LEN);
        let audio_len = data.len();

        // An APEv2 tag that contains what looks like frames.
        let mut ape_items = build_stream(&plain_frames(10), 9, FRAME_LEN);
        ape_items.extend_from_slice(&[0; 5]);

        let mut footer = Vec::new();
        footer.extend_from_slice(b"APETAGEX");
        footer.extend_from_slice(&2000u32.to_le_bytes());
        footer.extend_from_slice(&((ape_items.len() + 32) as u32).to_le_bytes());
        footer.extend_from_slice(&1u32.to_le_bytes());
        footer.extend_from_slice(&0u32.to_le_bytes());
        footer.extend_from_slice(&[0; 8]);

        data.extend_from_slice(&ape_items);
        data.extend_from_slice(&footer);

        // And an ID3v1 tag.
        let mut id3v1 = vec![0u8; 128];
        id3v1[..3].copy_from_slice(b"TAG");
        data.extend_from_slice(&id3v1);

        let mut reader = open_stream(data, true, FormatOptions::default());

        // The estimated duration only includes the audio.
        let n_frames = reader.tracks()[0].num_frames.unwrap();
        assert!(n_frames.abs_diff(n as u64 * 1152) <= 1152, "{n_frames}");

        // Only the audio frames are returned.
        let mut n_packets = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(frame_idx(&packet), n_packets);
            n_packets += 1;
        }

        assert_eq!(n_packets as usize, n);

        // As are the frames seeked over.
        assert!(matches!(
            reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new((n as i64 + 5) * 1152), track_id: 0 }
            ),
            Err(Error::SeekError(SeekErrorKind::OutOfRange))
        ));

        assert!(audio_len < reader.reader.byte_len().unwrap() as usize);
    }

    #[test]
    fn verify_estimated_duration_does_not_trim_the_end() {
        // A stream with padding in every frame: its average frame length is more than the nominal
        // length, which would underestimate the duration if it was used.
        let n = 100;
        let frames: Vec<_> =
            (0..n).map(|_| SyntheticFrame { main_data_begin: 0, padding: true }).collect();
        let data = build_stream(&frames, 9, FRAME_LEN);

        let mut reader = open_stream(data, true, FormatOptions::default());

        let mut n_packets = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            // No packet is trimmed.
            assert_eq!(packet.trim_start, Duration::ZERO);
            assert_eq!(packet.trim_end, Duration::ZERO);
            assert_eq!(packet.dur, Duration::from(1152u64));
            n_packets += 1;
        }

        assert_eq!(n_packets, n);
    }

    #[test]
    fn verify_prebuilt_index_gives_exact_duration() {
        // Frames of a variable bit-rate stream (alternating bit-rates, 128 and 64 kbps).
        let mut data = Vec::new();

        for idx in 0..120u32 {
            let (bitrate_idx, len) = if idx % 3 == 0 { (9u8, 418) } else { (5u8, 209) };
            let start = data.len();
            data.extend_from_slice(&[0xff, 0xfb, (bitrate_idx << 4) | 0x02, 0x00]);
            data.resize(start + len, 0);
            data[start + len - 4..].copy_from_slice(&idx.to_be_bytes());
        }

        let mut reader =
            open_stream(data.clone(), true, FormatOptions::default().prebuild_seek_index(true));

        assert_eq!(reader.tracks()[0].num_frames, Some(120 * 1152));

        // Seeks use the prebuilt index.
        assert!(reader.seek_index.points.len() > 1);

        let seeked = seek_to(&mut reader, SeekMode::Coarse, 100 * 1152);
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts, seeked.actual_ts);
        assert_eq!(i64::from(frame_idx(&packet)) * 1152, packet.pts.get());

        // The estimate isn't as good, but isn't terrible.
        let reader = open_stream(data, true, FormatOptions::default());
        let estimate = reader.tracks()[0].num_frames.unwrap();
        assert!(estimate.abs_diff(120 * 1152) < 120 * 1152 / 10, "{estimate}");
    }

    #[test]
    fn verify_free_format_stream() {
        let n = 60;

        // A free-format stream with 300 byte frames, padded one in 3.
        let frames: Vec<_> =
            (0..n).map(|i| SyntheticFrame { main_data_begin: 0, padding: i % 3 == 0 }).collect();
        let data = build_stream(&frames, 0, 300);

        // The stream is detected.
        let mut mss =
            MediaSourceStream::new(Box::new(Cursor::new(data.clone())), Default::default());
        assert!(matches!(
            MpaReader::score(ScopedStream::new(&mut mss, 16 * 1024)),
            Ok(Score::Supported(255))
        ));

        let mut reader = open_stream(data, true, FormatOptions::default());

        let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.sample_rate, Some(44100));

        // All frames are read.
        let mut n_packets = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(frame_idx(&packet), n_packets);
            assert_eq!(packet.data.len(), 300 + usize::from(n_packets % 3 == 0));
            n_packets += 1;
        }

        assert_eq!(n_packets as usize, n);

        // Seeking works.
        for required in [0, 1152 * 7 + 3, 1152 * 40, 1152 * 59 + 1151, 1152 * 20] {
            let seeked = seek_to(&mut reader, SeekMode::Accurate, required);
            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(packet.pts, seeked.actual_ts);
            assert_eq!(i64::from(frame_idx(&packet)) * 1152, packet.pts.get());
        }
    }
}
