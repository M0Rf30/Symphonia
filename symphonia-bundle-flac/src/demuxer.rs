// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Seek, SeekFrom};

use symphonia_core::support_format;

use symphonia_common::xiph::audio::flac::{MetadataBlockHeader, MetadataBlockType, StreamInfo};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, VerificationCheck, well_known::CODEC_ID_FLAC,
};
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::util::{SeekIndex, SeekSearchResult};
use symphonia_core::formats::well_known::FORMAT_ID_FLAC;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataBuilder, MetadataLog};
use symphonia_metadata::embedded::flac::*;

use log::{debug, info};

use super::frame::{BlockSequence, ChannelAssignment, read_frame_header};
use super::parser::PacketParser;

/// The FLAC start of stream marker: "fLaC" in ASCII.
const FLAC_STREAM_MARKER: [u8; 4] = *b"fLaC";

/// The FLAC frame synchronization codes for fixed-blocksize (0xfff8) and variable-blocksize
/// (0xfff9) streams. A stream that starts with one of these has no stream header (e.g., it was cut
/// out of the middle of another stream).
const FLAC_FRAME_SYNC_MARKERS: &[&[u8]] = &[&[0xff, 0xf8], &[0xff, 0xf9]];

/// The length of a FLAC STREAMINFO block.
const STREAM_INFO_BLOCK_LEN: usize = 34;

const FLAC_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_FLAC,
    short_name: "flac",
    long_name: "Free Lossless Audio Codec Native",
};

/// Free Lossless Audio Codec (FLAC) native frame reader.
pub struct FlacReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    index: Option<SeekIndex>,
    first_frame_offset: u64,
    parser: PacketParser,
}

impl<'s> FlacReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Read the first 4 bytes of the stream. Ideally this will be the FLAC stream marker.
        let marker = mss.read_quad_bytes()?;

        // A stream that begins directly with a frame has no metadata. Reconstruct the stream
        // information from the first frame header.
        if marker != FLAC_STREAM_MARKER {
            if is_frame_sync(marker) {
                mss.seek_buffered_rev(4);
                return FlacReader::init_headerless(mss);
            }

            return unsupported_error("flac: missing flac stream marker");
        }

        // Strictly speaking, the first metadata block must be a StreamInfo block. There is
        // no technical need for this from the reader's point of view. Additionally, if the
        // reader is fed a stream mid-way there is no StreamInfo block. Therefore, just read
        // all metadata blocks and handle the StreamInfo block as it comes.
        let flac = FlacReader::init_with_metadata(mss, opts)?;

        // Make sure that there is atleast one StreamInfo block.
        if flac.tracks.is_empty() {
            return decode_error("flac: no stream info block");
        }

        Ok(flac)
    }

    /// Initializes a reader for a stream that starts at a frame, without a stream marker or
    /// metadata blocks.
    fn init_headerless(mut reader: MediaSourceStream<'s>) -> Result<Self> {
        let extra_data = headerless_stream_info(&mut reader)?;

        let mut parser = PacketParser::default();
        let track = read_stream_info_block_data(extra_data, &mut parser)?;

        // Synchronize the packet parser to the first audio frame.
        let _ = parser.resync(&mut reader)?;

        let first_frame_offset = reader.pos();

        Ok(FlacReader {
            reader,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            attachments: Vec::new(),
            chapters: None,
            metadata: Default::default(),
            index: None,
            first_frame_offset,
            parser,
        })
    }

    /// Reads all the metadata blocks, returning a fully populated `FlacReader`.
    fn init_with_metadata(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut metadata_builder = MetadataBuilder::new(FLAC_METADATA_INFO);

        let mut reader = mss;
        let mut track = None;
        let mut attachments = Vec::new();
        let mut chapters = None;
        let mut index = None;
        let mut parser = Default::default();

        loop {
            let header = MetadataBlockHeader::read(&mut reader)?;

            // Create a scoped bytestream to error if the metadata block read functions exceed the
            // stated length of the block.
            let mut block_stream = ScopedStream::new(&mut reader, u64::from(header.block_len));

            match header.block_type {
                // The StreamInfo block is parsed into a track.
                MetadataBlockType::StreamInfo => {
                    // Only a single stream information block is allowed.
                    if track.is_none() {
                        track = Some(read_stream_info_block(&mut block_stream, &mut parser)?);
                    }
                    else {
                        return decode_error("flac: found more than one stream info block");
                    }
                }
                MetadataBlockType::Application => {
                    let vendor_data =
                        read_flac_application_block(&mut block_stream, header.block_len)?;
                    attachments.push(Attachment::VendorData(vendor_data));
                }
                // SeekTable blocks are parsed into a SeekIndex.
                MetadataBlockType::SeekTable => {
                    // Only a single seek table block is allowed.
                    if index.is_none() {
                        index =
                            Some(read_flac_seektable_block(&mut block_stream, header.block_len)?);
                    }
                    else {
                        return decode_error("flac: found more than one seek table block");
                    }
                }
                // VorbisComment blocks are parsed into Tags.
                MetadataBlockType::VorbisComment => {
                    read_flac_comment_block(&mut block_stream, &mut metadata_builder)?;
                }
                // Cuesheet blocks are parsed into Cues.
                MetadataBlockType::Cuesheet => {
                    // A cuesheet block must appear before the stream information block so that the
                    // timebase is known to calculate the cue times. This should always be the case
                    // since the stream information block must always be the first metadata block.
                    if let Some(tb) = track.as_ref().and_then(|track| track.time_base) {
                        chapters = Some(read_flac_cuesheet_block(&mut block_stream, tb)?);
                    }
                    else {
                        return decode_error("flac: cuesheet block before stream info");
                    }
                }
                // Picture blocks are read as Visuals.
                MetadataBlockType::Picture => {
                    metadata_builder.add_visual(read_flac_picture_block(&mut block_stream)?);
                }
                // Padding blocks are skipped.
                MetadataBlockType::Padding => {
                    block_stream.ignore_bytes(u64::from(header.block_len))?;
                }
                // Unknown block encountered. Skip these blocks as they may be part of a future
                // version of FLAC, but  print a message.
                MetadataBlockType::Unknown(id) => {
                    block_stream.ignore_bytes(u64::from(header.block_len))?;
                    info!("ignoring {} bytes of block width id={}.", header.block_len, id);
                }
            }

            // If the stated block length is longer than the number of bytes from the block read,
            // ignore the remaining unread data.
            let block_unread_len = block_stream.bytes_available();

            if block_unread_len > 0 {
                info!("under read block by {block_unread_len} bytes.");
                block_stream.ignore_bytes(block_unread_len)?;
            }

            // Exit when the last header is read.
            if header.is_last {
                break;
            }
        }

        // A single stream information block is mandatory. So it is an error for track to be `None`
        // after iterating over all metadata blocks.
        let Some(track) = track
        else {
            return decode_error("flac: missing stream info block");
        };

        // Commit any read metadata to the metadata log.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();
        metadata.push(metadata_builder.build());

        // Synchronize the packet parser to the first audio frame.
        let _ = parser.resync(&mut reader)?;

        // The first frame offset is the byte offset from the beginning of the stream after all the
        // metadata blocks have been read.
        let first_frame_offset = reader.pos();

        Ok(FlacReader {
            reader,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            attachments,
            chapters,
            metadata,
            index,
            first_frame_offset,
            parser,
        })
    }
}

impl Scoreable for FlacReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let marker = src.read_quad_bytes()?;

        if marker == FLAC_STREAM_MARKER {
            return Ok(Score::Supported(255));
        }

        // A bare frame synchronization code is a weak signal. Require the frame to have a valid
        // header, and that the frame itself is valid if it fits within the scoring window.
        if !is_frame_sync(marker) {
            return Ok(Score::Unsupported);
        }

        src.seek_buffered_rev(4);

        let extra_data = headerless_stream_info(&mut src)?;

        let mut parser = PacketParser::default();
        read_stream_info_block_data(extra_data, &mut parser)?;
        parser.resync(&mut src)?;

        match parser.parse(&mut src) {
            Ok(Some(_)) => Ok(Score::Supported(64)),
            // The frame may be larger than the scoring window, or the stream may be truncated. The
            // frame header was valid, so accept the stream.
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                Ok(Score::Supported(64))
            }
            _ => Ok(Score::Unsupported),
        }
    }
}

impl ProbeableFormat<'_> for FlacReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(FlacReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            support_format!(FLAC_FORMAT_INFO, &["flac"], &["audio/flac"], &[b"fLaC"]),
            support_format!(FLAC_FORMAT_INFO, &["flac"], &["audio/flac"], FLAC_FRAME_SYNC_MARKERS),
        ]
    }
}

impl FormatReader for FlacReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &FLAC_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.parser.parse(&mut self.reader)
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
        let Some(track) = self.tracks.first()
        else {
            return seek_error(SeekErrorKind::Unseekable);
        };

        // Get the timestamp of the desired audio frame.
        let ts = match to {
            // Frame timestamp given.
            SeekTo::Timestamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the track's timebase.
            SeekTo::Time { time, .. } => {
                // The timebase is required to calculate the timestamp.
                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                // If the timestamp overflows, the seek if out-of-range.
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        debug!("seeking to frame_ts={ts}");

        // Negative timestamp are invalid for FLAC.
        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // If the total number of frames in the stream is known, verify the desired frame timestamp
        // does not exceed it.
        if let Some(num_frames) = track.num_frames {
            // The timestamp is always positive.
            if ts.get() as u64 > num_frames {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        // If the reader supports seeking, coarsely seek to the nearest packet with a timestamp
        // lower than the desired timestamp using a binary search.
        if self.reader.is_seekable() {
            // The range formed by start_byte_offset..end_byte_offset defines an area where the
            // binary search for the packet containing the desired timestamp will be performed. The
            // lower bound is set to the byte offset of the first frame, while the upper bound is
            // set to the length of the stream.
            let mut start_byte_offset = self.first_frame_offset;
            let mut end_byte_offset = self.reader.seek(SeekFrom::End(0))?;

            // If there is an index, use it to refine the binary search range.
            if let Some(ref index) = self.index {
                // Search the index for the timestamp. Adjust the search based on the result.
                match index.search(ts) {
                    // Search from the start of stream up-to an ending point.
                    SeekSearchResult::Upper(upper) => {
                        end_byte_offset = self.first_frame_offset + upper.byte_offset;
                    }
                    // Search from a starting point up-to the end of the stream.
                    SeekSearchResult::Lower(lower) => {
                        start_byte_offset = self.first_frame_offset + lower.byte_offset;
                    }
                    // Search between two points of the stream.
                    SeekSearchResult::Range(lower, upper) => {
                        start_byte_offset = self.first_frame_offset + lower.byte_offset;
                        end_byte_offset = self.first_frame_offset + upper.byte_offset;
                    }
                    // Search the entire stream (default behaviour, so do nothing).
                    SeekSearchResult::Stream => (),
                }
            }

            // Binary search the range of bytes formed by start_by_offset..end_byte_offset for the
            // desired frame timestamp. When the difference of the range reaches 2x the maximum
            // frame size, exit the loop and search from the start_byte_offset linearly. The binary
            // search becomes inefficient when the range is small.
            while end_byte_offset - start_byte_offset > 2 * 8096 {
                let mid_byte_offset = (start_byte_offset + end_byte_offset) / 2;
                self.reader.seek(SeekFrom::Start(mid_byte_offset))?;

                let sync = self.parser.resync(&mut self.reader)?;

                if ts < sync.ts {
                    end_byte_offset = mid_byte_offset;
                }
                else if ts >= sync.ts && ts < sync.next_ts() {
                    debug!("seeked to ts={} (delta={})", sync.ts, sync.ts.saturating_delta(ts));

                    return Ok(SeekedTo { track_id: 0, actual_ts: sync.ts, required_ts: ts });
                }
                else {
                    start_byte_offset = mid_byte_offset;
                }
            }

            // The binary search did not find an exact frame, but the range has been narrowed. Seek
            // to the start of the range, and continue with a linear search.
            self.reader.seek(SeekFrom::Start(start_byte_offset))?;
        }

        // Linearly search the stream packet-by-packet for the packet that contains the desired
        // timestamp. This search is used to find the exact packet containing the desired timestamp
        // after the search range was narrowed by the binary search. It is also the ONLY way for a
        // unseekable stream to be "seeked" forward.
        let packet = loop {
            // Resync the FLAC frame parser.
            //
            // If the duration of the track is not known, then it is not possible to know if the
            // source has been truncated or not when an UnexpectedEof error occurs. Therefore, in
            // this case, return an out-of-range error. However, if the duration is known, then the
            // only reason why an UnexpectedEof would occur is if the source has been truncated
            // since the required timestamp was checked to be shorter than the duration earlier. In
            // this case the UnexpectedEof error should be passed-on to the caller.
            let sync = match self.parser.resync(&mut self.reader) {
                Ok(sync) => sync,
                Err(Error::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof
                        && track.num_frames.is_none() =>
                {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            // The desired timestamp precedes the current packet's timestamp.
            if ts < sync.ts {
                // Attempted to seek backwards on an unseekable stream.
                if !self.reader.is_seekable() {
                    return seek_error(SeekErrorKind::ForwardOnly);
                }
                // Overshot a regular seek, or the stream is corrupted, not necessarily an error
                // per-say.
                else {
                    break sync;
                }
            }
            // The desired timestamp is contained within the current packet.
            else if ts >= sync.ts && ts < sync.next_ts() {
                break sync;
            }

            // Advance the reader such that the next iteration will sync to a different frame.
            self.reader.read_byte()?;
        };

        debug!("seeked to packet_ts={} (delta={})", packet.ts, packet.ts.saturating_delta(ts));

        Ok(SeekedTo { track_id: 0, actual_ts: packet.ts, required_ts: ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

/// Reads a StreamInfo block and populates the reader with stream information.
fn read_stream_info_block<B: ReadBytes + FiniteStream>(
    reader: &mut B,
    parser: &mut PacketParser,
) -> Result<Track> {
    // Ensure the block length is correct for a stream information block before allocating a
    // buffer for it.
    if !StreamInfo::is_valid_size(reader.byte_len()) {
        return decode_error("flac: invalid stream info block size");
    }

    // Read the stream information block as a boxed slice so that it may be attached as extra
    // data on the codec parameters.
    let extra_data = reader.read_boxed_slice_exact(reader.byte_len() as usize)?;

    read_stream_info_block_data(extra_data, parser)
}

/// Returns true if the marker begins with a FLAC frame synchronization code.
fn is_frame_sync(marker: [u8; 4]) -> bool {
    marker[0] == 0xff && (marker[1] & 0xfe) == 0xf8
}

/// Reads the header of the frame at the current position, then rewinds to the start of the frame.
/// Returns an equivalent STREAMINFO block (without a total length or MD5), inferred from the
/// header, for a stream that has none.
fn headerless_stream_info<B: ReadBytes + SeekBuffered>(reader: &mut B) -> Result<Box<[u8]>> {
    let frame_pos = reader.pos();

    let sync = reader.read_be_u16()?;
    let header = read_frame_header(reader, sync);

    reader.seek_buffered(frame_pos);

    let header = header?;

    // The sample rate and bits per sample are not optional in a STREAMINFO block, so the frame
    // header must state them.
    let (Some(sample_rate), Some(bits_per_sample)) = (header.sample_rate, header.bits_per_sample)
    else {
        return unsupported_error("flac: frame header relies on a missing stream info block");
    };

    let num_channels = match header.channel_assignment {
        ChannelAssignment::Independent(num) => num,
        _ => 2,
    };

    // The nominal block length is that of the first frame for a fixed-blocksize stream (every
    // frame but the last is the same length). The block lengths of a variable-blocksize stream are
    // unknown.
    let (block_len_min, block_len_max) = match header.block_sequence {
        BlockSequence::ByFrame(_) => {
            let len = header.block_num_samples.max(16);
            (len, len)
        }
        BlockSequence::BySample(_) => (16, u16::MAX),
    };

    let mut data = Vec::with_capacity(STREAM_INFO_BLOCK_LEN);

    data.extend_from_slice(&block_len_min.to_be_bytes());
    data.extend_from_slice(&block_len_max.to_be_bytes());
    // Minimum and maximum frame byte lengths (unknown).
    data.extend_from_slice(&[0; 6]);
    // Sample rate (20 bits), channels - 1 (3 bits), bits per sample - 1 (5 bits), and total
    // samples (36 bits, unknown).
    let packed = (u64::from(sample_rate) << 44)
        | (u64::from(num_channels - 1) << 41)
        | (u64::from(bits_per_sample - 1) << 36);
    data.extend_from_slice(&packed.to_be_bytes());
    // MD5 (unknown).
    data.extend_from_slice(&[0; 16]);

    Ok(data.into_boxed_slice())
}

/// Parses a STREAMINFO block and populates the reader with stream information.
fn read_stream_info_block_data(extra_data: Box<[u8]>, parser: &mut PacketParser) -> Result<Track> {
    // Parse the stream info block.
    let info = StreamInfo::read(&mut BufReader::new(&extra_data))?;

    // Populate the codec parameters with the basic audio parameters of the track.
    let mut codec_params = AudioCodecParameters::new();

    codec_params
        .for_codec(CODEC_ID_FLAC)
        .with_extra_data(extra_data)
        .with_sample_rate(info.sample_rate)
        .with_bits_per_sample(info.bits_per_sample)
        .with_channels(info.channels.clone());

    if let Some(md5) = info.md5 {
        codec_params.with_verification_code(VerificationCheck::Md5(md5));
    }

    // Populate the track.
    let mut track = Track::new(0);

    track.with_codec_params(CodecParameters::Audio(codec_params));

    // Total samples per channel (also the total number of frames) is optional.
    if let Some(num_frames) = info.n_samples {
        track.with_num_frames(num_frames);
        // Duration equals the number of frames because the timebase is always 1 / sample rate.
        track.with_duration(Duration::from(num_frames));
    }

    // Reset the packet parser.
    parser.reset(info);

    Ok(track)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::checksum::{Crc8Ccitt, Crc16Ansi};
    use symphonia_core::formats::prelude::*;
    use symphonia_core::io::{MediaSourceStream, Monitor};

    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::formats::probe::Probe;

    use super::FlacReader;

    const BLOCK_LEN: u16 = 1000;

    /// Build a minimal mono, 8 kHz, 16-bit stream of fixed-blocksize VERBATIM frames. Each entry
    /// of `block_lens` is the length of a frame in samples. `min_block_len` is written verbatim to
    /// the STREAMINFO block.
    fn build_stream(min_block_len: u16, block_lens: &[u16]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();

        // STREAMINFO, last metadata block.
        out.extend_from_slice(&[0x80, 0, 0, 34]);
        out.extend_from_slice(&min_block_len.to_be_bytes());
        out.extend_from_slice(&BLOCK_LEN.to_be_bytes());
        out.extend_from_slice(&[0; 6]); // Frame byte length bounds (unknown).
        // 8000 Hz (20 bits), 1 channel (3 bits), 16 bits per sample (5 bits), 0 total samples
        // (36 bits).
        let packed: u64 = (8000u64 << 44) | (15u64 << 36);
        out.extend_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&[0; 16]); // MD5.

        for (n, &len) in block_lens.iter().enumerate() {
            let mut frame = vec![0xff, 0xf8, 0x74, 0x08, n as u8];
            frame.extend_from_slice(&(len - 1).to_be_bytes());
            let mut crc8 = Crc8Ccitt::new(0);
            crc8.process_buf_bytes(&frame);
            frame.push(crc8.crc());

            // VERBATIM subframe.
            frame.push(0x02);
            for i in 0..len {
                frame.extend_from_slice(&(i as i16).wrapping_mul(7).to_be_bytes());
            }

            let mut crc16 = Crc16Ansi::new(0);
            crc16.process_buf_bytes(&frame);
            frame.extend_from_slice(&crc16.crc().to_be_bytes());
            out.extend_from_slice(&frame);
        }

        out
    }

    fn open(data: Vec<u8>) -> FlacReader<'static> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        FlacReader::try_new(mss, Default::default()).unwrap()
    }

    fn read_all(reader: &mut FlacReader<'_>) -> Vec<(i64, u64)> {
        let mut pkts = Vec::new();
        while let Some(pkt) = reader.next_packet().unwrap() {
            pkts.push((pkt.pts.get(), pkt.dur.get()));
        }
        pkts
    }

    // Upstream issue #564.
    #[test]
    fn verify_seek_to_start_after_eof_restarts_stream() {
        let mut reader = open(build_stream(BLOCK_LEN, &[BLOCK_LEN; 4]));

        let first = read_all(&mut reader);
        assert_eq!(first.len(), 4);

        let to = SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0 };
        assert_eq!(reader.seek(SeekMode::Accurate, to).unwrap().actual_ts, Timestamp::new(0));

        assert_eq!(read_all(&mut reader), first);
    }

    // Upstream issue #554.
    #[test]
    fn verify_unknown_min_block_length_is_accepted() {
        let mut reader = open(build_stream(0, &[BLOCK_LEN, BLOCK_LEN, BLOCK_LEN, 500]));

        let expected = vec![(0, 1000), (1000, 1000), (2000, 1000), (3000, 500)];
        assert_eq!(read_all(&mut reader), expected);

        let to = SeekTo::Timestamp { ts: Timestamp::new(2500), track_id: 0 };
        assert_eq!(reader.seek(SeekMode::Accurate, to).unwrap().actual_ts, Timestamp::new(2000));
        assert_eq!(read_all(&mut reader), expected[2..]);
    }

    // Upstream issue #372.
    #[test]
    fn verify_stream_without_header_is_readable() {
        // Drop the stream marker and the STREAMINFO block, and the first frame.
        let full = build_stream(BLOCK_LEN, &[BLOCK_LEN; 4]);
        let frame_len = (full.len() - 42) / 4;
        let data = full[42 + frame_len..].to_vec();

        let mut reader = open(data.clone());

        let tracks = reader.tracks();
        assert_eq!(tracks.len(), 1);
        let Some(CodecParameters::Audio(params)) = tracks[0].codec_params.as_ref()
        else {
            panic!("expected audio codec parameters");
        };
        assert_eq!(params.sample_rate, Some(8000));
        assert_eq!(params.bits_per_sample, Some(16));

        // The timestamps retain their position in the original stream.
        assert_eq!(read_all(&mut reader), vec![(1000, 1000), (2000, 1000), (3000, 1000)]);

        let to = SeekTo::Timestamp { ts: Timestamp::new(2500), track_id: 0 };
        assert_eq!(reader.seek(SeekMode::Accurate, to).unwrap().actual_ts, Timestamp::new(2000));

        // The stream is detected by the probe.
        let mut probe = Probe::default();
        probe.register_format::<FlacReader<'_>>();
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        let reader =
            probe.probe(&Default::default(), mss, Default::default(), Default::default()).unwrap();
        assert_eq!(reader.format_info().short_name, "flac");
    }
}
