// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom};

use symphonia_core::errors::{Error, Result, SeekErrorKind};
use symphonia_core::errors::{decode_error, reset_error, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_OGG;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog, MetadataSideData};
use symphonia_core::support_format;
use symphonia_core::units::Time;

use log::{debug, info, warn};

use super::common::SideData;
use super::logical::LogicalStream;
use super::mappings;
use super::page::*;
use super::physical;

const OGG_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_OGG, short_name: "ogg", long_name: "Ogg" };

/// A link (physical bitstream) of a chained Ogg stream.
#[derive(Copy, Clone, Debug)]
struct Link {
    /// The position of the first byte of the link's first page.
    byte_start: u64,
    /// The start time of the link in the chain's timeline in nanoseconds.
    start_ns: i128,
    /// The duration of the link in nanoseconds.
    duration_ns: i128,
}

/// The maximum number of links of a chained stream that are scanned when the stream is opened.
const MAX_CHAIN_LINKS: usize = 1 << 14;

/// A physical bitstream (link) that has been fully probed.
struct PhysicalStream {
    /// `LogicalStream` for each serial.
    streams: BTreeMap<u32, LogicalStream>,
    /// The position of the first byte of the first bitstream page of the physical stream.
    byte_range_start: u64,
    /// The position of the first byte after the physical stream, if available.
    byte_range_end: Option<u64>,
}

/// OGG demultiplexer.
///
/// `OggReader` implements a demuxer for Xiph's OGG container format.
///
/// # Chained streams
///
/// A chained Ogg stream is a sequence of independent physical bitstreams (links), each with its
/// own set of logical streams (tracks, and typically serial numbers), codec parameters, and
/// metadata. The reader exposes the tracks of one link at a time. Timestamps (`pts`, and
/// `SeekTo::Timestamp`) are always relative to the start of the current link.
///
/// While reading sequentially, `Error::ResetRequired` is returned by `next_packet` when the next
/// link starts. The tracks must then be re-examined and the decoders re-created.
///
/// If the media source is seekable and its length is known, all links are located when the
/// stream is opened. In that case, `MediaInfo` describes the whole chain (the duration is the sum
/// of the durations of all links, in nanoseconds), and `SeekTo::Time` is interpreted relative to
/// the start of the chain. A time seek that targets a different link than the current one
/// switches to that link and returns `Error::ResetRequired`. After re-examining the tracks and
/// re-creating the decoders, the same seek must be repeated; it then completes within the link.
pub struct OggReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    /// The page reader.
    pages: PageReader,
    /// `LogicalStream` for each serial.
    streams: BTreeMap<u32, LogicalStream>,
    /// The position of the first byte of the current physical stream.
    phys_byte_range_start: u64,
    /// The position of the first byte of the next physical stream, if available.
    phys_byte_range_end: Option<u64>,
    /// The links of a chained stream. Empty unless the stream is chained and seekable.
    links: Vec<Link>,
    /// The index of the current link in `links`.
    link_idx: usize,
}

impl<'s> OggReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // A seekback buffer equal to the maximum OGG page size is required for this reader.
        mss.ensure_seekback_buffer(OGG_PAGE_MAX_SIZE);

        // The position of the first link.
        let link0_byte_start = mss.pos();

        let pages = PageReader::try_new(&mut mss)?;

        if !pages.header().is_first_page {
            return unsupported_error("ogg: page is not marked as first");
        }

        let mut ogg = OggReader {
            reader: mss,
            media_info: Default::default(),
            tracks: Default::default(),
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            streams: Default::default(),
            pages,
            phys_byte_range_start: 0,
            phys_byte_range_end: None,
            links: Vec::new(),
            link_idx: 0,
        };

        ogg.start_new_physical_stream()?;
        ogg.scan_links(link0_byte_start)?;

        Ok(ogg)
    }

    /// If the stream is seekable and chained, locates all the links and builds the timeline of
    /// the chain. The reader is left at the start of the first link.
    fn scan_links(&mut self, link0_byte_start: u64) -> Result<()> {
        let Some(total_len) = self.reader.byte_len()
        else {
            return Ok(());
        };

        if !self.reader.is_seekable() {
            return Ok(());
        }

        // The first link was already probed.
        let mut links = Vec::new();
        let mut start_ns = 0i128;
        let mut next_pos = self.phys_byte_range_end;
        let mut complete = true;

        let Some(duration_ns) = Self::link_duration_ns(&self.tracks)
        else {
            return Ok(());
        };

        links.push(Link { byte_start: link0_byte_start, start_ns, duration_ns });
        start_ns += duration_ns;

        // Probe each of the remaining links.
        while let Some(pos) = next_pos {
            // The next link must start after the current link, and before the end of the source.
            let cur_start = links.last().map(|l| l.byte_start).unwrap_or(0);

            if pos >= total_len || pos <= cur_start {
                break;
            }

            if links.len() >= MAX_CHAIN_LINKS {
                complete = false;
                break;
            }

            if self.reader.seek(SeekFrom::Start(pos)).is_err()
                || self.pages.next_page(&mut self.reader).is_err()
                || !self.pages.header().is_first_page
            {
                break;
            }

            let Ok(link) = self.read_physical_stream(false)
            else {
                break;
            };

            let tracks: Vec<Track> = link.streams.values().map(|s| s.track().clone()).collect();

            let Some(duration_ns) = Self::link_duration_ns(&tracks)
            else {
                complete = false;
                break;
            };

            links.push(Link { byte_start: pos, start_ns, duration_ns });
            start_ns += duration_ns;
            next_pos = link.byte_range_end;
        }

        // Return to the first bitstream page of the first link.
        self.reader.seek(SeekFrom::Start(self.phys_byte_range_start))?;
        self.pages.next_page(&mut self.reader)?;

        // Only a chain with a complete timeline can be seeked.
        if complete && links.len() > 1 {
            debug!("ogg: chained stream with {} links, duration={start_ns}ns", links.len());

            let mut media_info = MediaInfo::new();

            if let Some(tb) = TimeBase::try_new(1, 1_000_000_000) {
                media_info.with_time_base(tb);
                media_info
                    .with_duration(Duration::new(u64::try_from(start_ns).unwrap_or(u64::MAX)));
                media_info.start_ts = Timestamp::ZERO;

                self.media_info = media_info;
                self.links = links;
                self.link_idx = 0;
            }
        }

        Ok(())
    }

    /// Gets the duration, in nanoseconds, of the longest track of a link.
    fn link_duration_ns(tracks: &[Track]) -> Option<i128> {
        tracks
            .iter()
            .filter_map(|track| {
                let tb = track.time_base?;
                let num_frames = track.num_frames?;
                Some(tb.calc_duration(Duration::new(num_frames))?.as_nanos())
            })
            .max()
    }

    /// Switches to a link of a chained stream. The reader is left at the start of the link.
    fn switch_link(&mut self, link_idx: usize) -> Result<()> {
        let byte_start = self.links[link_idx].byte_start;

        self.reader.seek(SeekFrom::Start(byte_start))?;
        self.pages.next_page(&mut self.reader)?;

        if !self.pages.header().is_first_page {
            return decode_error("ogg: expected the first page of a chained stream");
        }

        self.start_new_physical_stream()?;
        self.link_idx = link_idx;

        Ok(())
    }

    /// Converts a time relative to the start of the chain to a time relative to the start of the
    /// current link. If the time is within another link, switches to that link and returns
    /// `Error::ResetRequired`.
    fn chain_time_to_link_time(&mut self, time: Time) -> Result<Time> {
        let ns = time.as_nanos();

        let Some(last) = self.links.last()
        else {
            return Ok(time);
        };

        if ns < 0 || ns > last.start_ns + last.duration_ns {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // Find the link that contains the time.
        let link_idx = self.links.partition_point(|link| link.start_ns <= ns).saturating_sub(1);

        if link_idx != self.link_idx {
            self.switch_link(link_idx)?;
            return reset_error();
        }

        Time::try_from_nanos_i128(ns - self.links[link_idx].start_ns)
            .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))
    }

    fn read_page(&mut self) -> Result<()> {
        // Try reading pages until a page is successfully read, or an IO error.
        loop {
            match self.pages.try_next_page(&mut self.reader) {
                Ok(_) => break,
                Err(Error::IoError(e)) => return Err(Error::from(e)),
                Err(e) => {
                    warn!("{e}");
                }
            }
        }

        let page = self.pages.page();

        // If the page is marked as a first page, then try to start a new physical stream.
        if page.header.is_first_page {
            self.start_new_physical_stream()?;

            // Keep track of the current link in a chained stream.
            if !self.links.is_empty() {
                self.link_idx = (self.link_idx + 1).min(self.links.len() - 1);
            }

            return reset_error();
        }

        if let Some(stream) = self.streams.get_mut(&page.header.serial) {
            // TODO: Process side data.
            let _side_data = stream.read_page(&page)?;
        }
        else {
            // If there is no associated logical stream with this page, then this is a
            // completely random page within the physical stream. Discard it.
        }

        Ok(())
    }

    fn have_all_streams_read_last_page(&self) -> bool {
        self.streams.iter().all(|(_, stream)| stream.has_read_last_page().unwrap_or(true))
    }

    fn peek_logical_packet(&self) -> Option<&Packet> {
        let page = self.pages.page();

        if let Some(stream) = self.streams.get(&page.header.serial) {
            stream.peek_packet()
        }
        else {
            None
        }
    }

    fn discard_logical_packet(&mut self) {
        let page = self.pages.page();

        // Consume a packet from the logical stream belonging to the current page.
        if let Some(stream) = self.streams.get_mut(&page.header.serial) {
            stream.consume_packet();
        }
    }

    fn next_logical_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let page = self.pages.page();

            // Read the next packet. Packets are only ever buffered in the logical stream of the
            // current page.
            if let Some(stream) = self.streams.get_mut(&page.header.serial) {
                if let Some(packet) = stream.next_packet() {
                    return Ok(Some(packet));
                }
            }

            match self.read_page() {
                Ok(_) => (),
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Check that all logical streams have read their last page.
                    if self.have_all_streams_read_last_page() {
                        // All streams read their last page. End of stream has been reached.
                        return Ok(None);
                    }

                    // A stream did not reach its last page. Consider this an unexpected EOF.
                    return Err(Error::IoError(err));
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn do_seek(&mut self, serial: u32, required_ts: Timestamp) -> Result<SeekedTo> {
        // The maximum duration between two random access points of the stream being seeked.
        let mut rap = match self.streams.get(&serial) {
            Some(s) => s.max_rap_period(),
            None => return decode_error("ogg: serial not found in streams"),
        };

        loop {
            let seeked = self.do_seek_with_rap(serial, required_ts, rap)?;

            // Reading the pages of the seek may reveal something about the stream that changes
            // its random access period (e.g. a stream believed to be CELT-only is found to
            // contain SILK frames, which need a longer pre-roll). If so, seek again with the
            // updated period.
            let new_rap = match self.streams.get(&serial) {
                Some(s) => s.max_rap_period(),
                None => return Ok(seeked),
            };

            if new_rap <= rap {
                return Ok(seeked);
            }

            debug!("seek: random access period grew from {rap} to {new_rap}, seeking again");
            rap = new_rap;
        }
    }

    fn do_seek_with_rap(
        &mut self,
        serial: u32,
        required_ts: Timestamp,
        rap: Duration,
    ) -> Result<SeekedTo> {
        // The stream being seeked.
        let stream = match self.streams.get_mut(&serial) {
            Some(s) => s,
            None => return decode_error("ogg: serial not found in streams"),
        };

        // Subtract the maximum duration between random access points from the required timestamp.
        // This ensures any frames that need to be consumed or discarded by the decoder on reset
        // are dealt with before the required timestamp.
        let target_ts = required_ts.saturating_sub(rap);

        debug!("seek: target_ts={target_ts} (rap={})", required_ts.saturating_delta(target_ts));

        // If the reader is seekable, then use the bisection method to coarsely seek to the nearest
        // page that ends before the required timestamp.
        if self.reader.is_seekable() {
            // Bisection method byte ranges. When these two values are equal, the bisection has
            // converged on the position of the correct page.
            let mut start_byte_pos = self.phys_byte_range_start;
            let mut end_byte_pos = match self.phys_byte_range_end {
                Some(v) => v,
                None => return decode_error("ogg: physical byte range end is unknown"),
            };

            // Bisect the stream while the byte range is large. For smaller ranges, a linear scan is
            // faster than having the the binary search converge.
            while end_byte_pos - start_byte_pos > 2 * OGG_PAGE_MAX_SIZE as u64 {
                // Find the middle of the upper and lower byte search range.
                let mid_byte_pos = (start_byte_pos + end_byte_pos) / 2;

                // Seek to the middle of the byte range.
                self.reader.seek(SeekFrom::Start(mid_byte_pos))?;

                // Read the next page.
                match self.pages.next_page_for_serial(&mut self.reader, serial) {
                    Ok(_) => (),
                    _ => {
                        // No more pages for the stream from the mid-point onwards.
                        debug!(
                            "seek: bisect step: byte_range=\
                            [{start_byte_pos}, {mid_byte_pos}, {end_byte_pos}]",
                        );

                        end_byte_pos = mid_byte_pos;
                        continue;
                    }
                }

                // Probe the page to get the start and end timestamp.
                let (start_ts, end_ts) = stream.inspect_page(&self.pages.page());

                debug!(
                    "seek: bisect step: page={{ start_ts={start_ts}, end_ts={end_ts} }} \
                    byte_range=[{start_byte_pos}, {mid_byte_pos}, {end_byte_pos}]",
                );

                if target_ts < start_ts {
                    // The required timestamp is less-than the timestamp of the first sample in the
                    // page. Update the upper bound and bisect again.
                    end_byte_pos = mid_byte_pos;
                }
                else if target_ts > end_ts {
                    // The required timestamp is greater-than the timestamp of the final sample in
                    // the in the page. Update the lower bound and bisect again.
                    start_byte_pos = mid_byte_pos;
                }
                else {
                    // The sample with the required timestamp is contained in the page. The
                    // bisection has converged on the correct page so stop the bisection.
                    start_byte_pos = mid_byte_pos;
                    end_byte_pos = mid_byte_pos;
                    break;
                }
            }

            // If the bisection did not converge, then the linear search must continue from the
            // lower-bound (start) position of what would've been the next iteration of bisection.
            if start_byte_pos != end_byte_pos {
                self.reader.seek(SeekFrom::Start(start_byte_pos))?;

                match self.pages.next_page_for_serial(&mut self.reader, serial) {
                    Ok(_) => (),
                    _ => return seek_error(SeekErrorKind::OutOfRange),
                }
            }

            // Reset all logical bitstreams since the physical stream will be reading from a new
            // location now.
            let page_seq = self.pages.header().sequence;

            for (&s, stream) in self.streams.iter_mut() {
                if s != serial {
                    stream.reset();
                }
                else {
                    stream.reset_for_seek(page_seq);

                    // Read in the current page since it contains our timestamp.
                    stream.read_page(&self.pages.page())?;
                }
            }
        }

        // Consume packets until reaching the desired timestamp.
        let actual_ts = loop {
            match self.peek_logical_packet() {
                Some(packet) if packet.track_id == serial => {
                    match packet.pts.checked_add(packet.dur) {
                        Some(next_packet_ts) if next_packet_ts < target_ts => (),
                        // Packet exceeds the requested timestamp, or the representable range.
                        _ => break packet.pts,
                    };

                    self.discard_logical_packet();
                }
                // The packet does not belong to the stream being seeked. Discard it so that the
                // next packet, or page, may be examined. Peeking alone makes no progress.
                Some(_) => self.discard_logical_packet(),
                _ => match self.read_page() {
                    Ok(_) => (),
                    Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // If all streams have read their last page, then the seek was out-of-range.
                        if self.have_all_streams_read_last_page() {
                            return seek_error(SeekErrorKind::OutOfRange);
                        }

                        // If a stream did not read its last page then this is an unexpected EOF.
                        return Err(Error::IoError(err));
                    }
                    Err(err) => return Err(err),
                },
            }
        };

        debug!(
            "seeked track={:#x} to packet_ts={} (delta={})",
            serial,
            actual_ts,
            actual_ts.saturating_delta(required_ts),
        );

        Ok(SeekedTo { track_id: serial, actual_ts, required_ts })
    }

    /// Starts a new physical stream. The current page must be a first page.
    fn start_new_physical_stream(&mut self) -> Result<()> {
        let phys = self.read_physical_stream(true)?;

        // At this point it can safely be assumed that a new physical stream is starting.

        // Clear the existing track listing.
        self.tracks.clear();

        // Add a track for each logical stream.
        for (&serial, stream) in phys.streams.iter() {
            // Warn if the track is not ready. This should not happen if the physical stream was
            // muxed properly.
            if !stream.is_ready() {
                warn!("track for serial={serial:#x} may not be ready");
            }

            self.tracks.push(stream.track().clone());
        }

        // Update media information. The media information of a chained stream describes the
        // entire chain.
        if self.links.is_empty() {
            self.media_info = MediaInfo::from_tracks(&self.tracks);
        }

        // Replace all logical streams with the new set.
        self.streams = phys.streams;

        // Store the lower and upper byte boundaries of the physical stream for seeking.
        self.phys_byte_range_start = phys.byte_range_start;
        self.phys_byte_range_end = phys.byte_range_end;

        Ok(())
    }

    /// Reads and probes a physical stream. The current page must be a first page. If
    /// `update_side_data` is false, the metadata and chapters of the stream are discarded.
    fn read_physical_stream(&mut self, update_side_data: bool) -> Result<PhysicalStream> {
        // The new mapper set.
        let mut streams = BTreeMap::<u32, LogicalStream>::new();

        // The start of page position.
        let mut byte_range_start = self.reader.pos();

        // Pre-condition: This function is only called when the current page is marked as a
        // first page.
        assert!(self.pages.header().is_first_page);

        info!("starting new physical stream");

        // The first page of each logical stream, marked with the first page flag, must contain the
        // identification packet for the encapsulated codec bitstream. The first page for each
        // logical stream from the current logical stream group must appear before any other pages.
        // That is to say, if there are N logical streams, then the first N pages must contain the
        // identification packets for each respective logical stream.
        loop {
            let header = self.pages.header();

            if !header.is_first_page {
                break;
            }

            byte_range_start = self.reader.pos();

            // There should only be a single packet, the identification packet, in the first page.
            if let Some(pkt) = self.pages.first_packet() {
                // If a stream mapper has been detected, create a logical stream with it.
                if let Some(mapper) = mappings::detect(header.serial, pkt)? {
                    info!(
                        "selected {} mapper for stream with serial={:#x}",
                        mapper.name(),
                        header.serial
                    );

                    streams.insert(header.serial, LogicalStream::new(mapper));
                }
            }

            // Read the next page.
            self.pages.try_next_page(&mut self.reader)?;
        }

        // Each logical stream may contain additional header packets after the identification packet
        // that contains format-relevant information such as setup and metadata. These packets,
        // for all logical streams, should be grouped together after the identification packets.
        // Reading pages consumes these headers and returns any relevant data as side data. Read
        // pages until all headers are consumed and the first bitstream packets are buffered.
        loop {
            let page = self.pages.page();

            if let Some(stream) = streams.get_mut(&page.header.serial) {
                let side_data = stream.read_page_init(&page, true)?;

                // Consume each piece of side data.
                for data in side_data {
                    if !update_side_data {
                        break;
                    }

                    match data {
                        SideData::Metadata { rev, side_data } => {
                            self.metadata.push(rev);

                            // Process side data.
                            for data in side_data {
                                if let MetadataSideData::Chapters(chapters) = data {
                                    self.chapters = Some(chapters);
                                }
                            }
                        }
                    }
                }

                if stream.has_packets() {
                    break;
                }
            }

            // The current page has been consumed and we're committed to reading a new one. Record
            // the end of the current page.
            byte_range_start = self.reader.pos();

            self.pages.try_next_page(&mut self.reader)?;
        }

        // Probe the logical streams for their start and end pages.
        physical::probe_stream_start(
            &mut self.reader,
            &mut self.pages,
            &mut streams,
            byte_range_start,
        )?;

        let mut byte_range_end = Default::default();

        // If the media source stream is seekable, then try to determine the duration of each
        // logical stream, and the length in bytes of the physical stream.
        if self.reader.is_seekable() {
            if let Some(total_len) = self.reader.byte_len() {
                byte_range_end = physical::probe_stream_end(
                    &mut self.reader,
                    &mut self.pages,
                    &mut streams,
                    byte_range_start,
                    total_len,
                )?;
            }
        }

        Ok(PhysicalStream { streams, byte_range_start, byte_range_end })
    }
}

impl Scoreable for OggReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for OggReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(OggReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            OGG_FORMAT_INFO,
            &["ogg", "ogv", "oga", "ogx", "ogm", "spx", "opus"],
            &["video/ogg", "audio/ogg", "application/ogg"],
            &[b"OggS"]
        )]
    }
}

impl FormatReader for OggReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &OGG_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.next_logical_packet()
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
        let (required_ts, serial) = match to {
            // Frame timestamp given.
            SeekTo::Timestamp { ts, track_id } => {
                // Check if the user provided an invalid track ID.
                if let Some(stream) = self.streams.get(&track_id) {
                    let track = stream.track();

                    // Timestamp lower-bound out-of-range.
                    if ts < track.start_ts {
                        return seek_error(SeekErrorKind::OutOfRange);
                    }

                    // Timestamp upper-bound out-of-range.
                    if let Some(num_frames) = track.num_frames {
                        // The number of frames excludes the delay frames at the start.
                        let max_ts = track
                            .start_ts
                            .checked_add(Duration::from(u64::from(track.delay.unwrap_or(0))))
                            .and_then(|ts| ts.checked_add(Duration::from(num_frames)))
                            .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                        if ts > max_ts {
                            return seek_error(SeekErrorKind::OutOfRange);
                        }
                    }
                }
                else {
                    return seek_error(SeekErrorKind::InvalidTrack);
                }

                (ts, track_id)
            }
            // Time value given, calculate frame timestamp from sample rate.
            SeekTo::Time { time, track_id } => {
                // Get the track serial.
                let serial = if let Some(serial) = track_id {
                    serial
                }
                else if let Some(first_track) = self.tracks.first() {
                    first_track.id
                }
                else {
                    // No tracks.
                    return seek_error(SeekErrorKind::Unseekable);
                };

                // The track must belong to the current link.
                if !self.streams.contains_key(&serial) {
                    return seek_error(SeekErrorKind::InvalidTrack);
                }

                // In a chained stream, the time is relative to the start of the chain.
                let time = self.chain_time_to_link_time(time)?;

                // Convert the time to a timestamp.
                let ts = if let Some(stream) = self.streams.get(&serial) {
                    let track = stream.track();

                    // The timebase is required to calculate the timestamp.
                    let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                    // If the timestamp overflows, the seek if out-of-range.
                    let ts = tb
                        .calc_timestamp(time)
                        .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                    // Timestamp lower-bound out-of-range.
                    if ts < track.start_ts {
                        return seek_error(SeekErrorKind::OutOfRange);
                    }

                    // Timestamp upper-bound out-of-range.
                    if let Some(num_frames) = track.num_frames {
                        // The number of frames excludes the delay frames at the start.
                        let max_ts = track
                            .start_ts
                            .checked_add(Duration::from(u64::from(track.delay.unwrap_or(0))))
                            .and_then(|ts| ts.checked_add(Duration::from(num_frames)))
                            .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                        if ts > max_ts {
                            return seek_error(SeekErrorKind::OutOfRange);
                        }
                    }

                    ts
                }
                else {
                    // No mapper for track. The user provided a bad track ID.
                    return seek_error(SeekErrorKind::InvalidTrack);
                };

                (ts, serial)
            }
        };

        debug!("seeking track={serial:#x} to frame_ts={required_ts}");

        // Do the actual seek.
        self.do_seek(serial, required_ts)
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}
