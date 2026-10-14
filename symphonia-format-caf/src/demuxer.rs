// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::chunks::*;
use log::{debug, error, info, warn};
use std::{
    io::{Seek, SeekFrom},
    num::NonZero,
};
use symphonia_core::{
    audio::{ChannelLabel, Channels, Position},
    codecs::{
        CodecParameters,
        audio::{
            well_known::{
                CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_MP1, CODEC_ID_MP2,
                CODEC_ID_MP3, CODEC_ID_OPUS,
            },
            *,
        },
    },
    errors::{Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error},
    formats::{
        prelude::*,
        probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable},
        well_known::FORMAT_ID_CAF,
    },
    io::*,
    meta::{
        Chapter, ChapterGroup, ChapterGroupItem, Metadata, MetadataBuilder, MetadataInfo,
        MetadataLog, Tag, well_known::METADATA_ID_CAF,
    },
    support_format,
    units::{TimeBase, Timestamp},
};

use crate::tags::push_tags;
use symphonia_common::mpeg::formats::*;

const MAX_FRAMES_PER_PACKET: u64 = 1152;

const CAF_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_CAF, short_name: "caf", long_name: "Core Audio Format" };

const CAF_METADATA_INFO: MetadataInfo =
    MetadataInfo { metadata: METADATA_ID_CAF, short_name: "caf", long_name: "Core Audio Format" };

/// Core Audio Format (CAF) format reader.
///
/// `CafReader` implements a demuxer for Core Audio Format containers.
pub struct CafReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    data_start_pos: u64,
    data_len: Option<u64>,
    packet_info: PacketInfo,
}

enum PacketInfo {
    Unknown,
    FixedAudioPacket {
        bytes_per_packet: NonZero<u32>,
        frames_per_packet: NonZero<u32>,
        /// The timestamp of the first packet. Negative if the stream has priming frames.
        start_pts: Timestamp,
        /// The timestamp after the last valid frame, if known.
        end_pts: Option<Timestamp>,
    },
    VariableAudioPacket {
        packets: Vec<CafPacket>,
        current_packet_index: usize,
        /// The timestamp after the last valid frame.
        end_pts: Option<Timestamp>,
    },
}

impl Scoreable for CafReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for CafReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(CafReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(CAF_FORMAT_INFO, &["caf"], &["audio/x-caf"], &[b"caff"])]
    }
}

impl FormatReader for CafReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &CAF_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        match &mut self.packet_info {
            PacketInfo::FixedAudioPacket {
                bytes_per_packet,
                frames_per_packet,
                start_pts,
                end_pts,
            } => {
                let pos = self.reader.pos();
                let data_pos = pos - self.data_start_pos;

                let bytes_per_packet = u64::from(bytes_per_packet.get());
                let frames_per_packet = u64::from(frames_per_packet.get());

                // frames_per_packet == 1 means uncompressed data which we want to chunk into
                // MAX_FRAMES_PER_PACKET
                let max_bytes_to_read = if frames_per_packet == 1 {
                    bytes_per_packet * MAX_FRAMES_PER_PACKET
                }
                else {
                    bytes_per_packet
                };

                let bytes_remaining = if let Some(data_len) = self.data_len {
                    data_len.saturating_sub(data_pos)
                }
                else {
                    max_bytes_to_read
                };

                if bytes_remaining == 0 {
                    return Ok(None);
                }

                let bytes_to_read = max_bytes_to_read.min(bytes_remaining);

                // Calculate the packet duration.
                let Some(dur) = (bytes_to_read / bytes_per_packet)
                    .checked_mul(frames_per_packet)
                    .map(Duration::new)
                else {
                    warn!("packet duration exceeds maximum representable duration");
                    return Ok(None);
                };

                // Calculate the packet PTS by offsetting the duration read so far from the start
                // PTS.
                let Some(pts) = (data_pos / bytes_per_packet)
                    .checked_mul(frames_per_packet)
                    .and_then(|offset| start_pts.checked_add(Duration::from(offset)))
                else {
                    warn!("media exceeds maximum representable duration");
                    return Ok(None);
                };

                // If the length of the audio data is unknown, the end of the stream is the end of
                // the data. A trailing partial packet is discarded.
                let buf = match self.reader.read_boxed_slice(bytes_to_read as usize) {
                    Ok(buf) => buf,
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(None);
                    }
                    Err(err) => return Err(err.into()),
                };
                let num_packets = buf.len() as u64 / bytes_per_packet;

                if num_packets == 0 {
                    return Ok(None);
                }

                let (buf, dur) = if num_packets < bytes_to_read / bytes_per_packet {
                    let dur = Duration::new(num_packets * frames_per_packet);
                    (buf[..(num_packets * bytes_per_packet) as usize].into(), dur)
                }
                else {
                    (buf, dur)
                };

                // The priming and padding frames are trimmed from compressed packets.
                let packet = PacketBuilder::new()
                    .track_id(0)
                    .pts(pts)
                    .trimmed_dur(dur, *end_pts)
                    .data(buf)
                    .build();

                Ok(Some(packet))
            }
            PacketInfo::VariableAudioPacket { packets, current_packet_index, end_pts } => {
                let Some(packet) = packets.get(*current_packet_index)
                else {
                    return Ok(None);
                };

                // The packet size is untrusted. Do not allocate more than the stream contains.
                if let Some(byte_len) = self.reader.byte_len() {
                    if self.reader.pos().saturating_add(packet.size) > byte_len {
                        return Ok(None);
                    }
                }

                let buffer = match self.reader.read_boxed_slice(packet.size as usize) {
                    Ok(buffer) => buffer,
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(None);
                    }
                    Err(err) => return Err(err.into()),
                };

                *current_packet_index += 1;

                // The priming and padding frames are trimmed from the packets.
                let packet = PacketBuilder::new()
                    .track_id(0)
                    .pts(packet.start_frame)
                    .trimmed_dur(packet.frames, *end_pts)
                    .data(buffer)
                    .build();

                Ok(Some(packet))
            }
            PacketInfo::Unknown => decode_error("caf: missing packet info"),
        }
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
        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                // The timebase is required to calculate the timestamp.
                let tb = self.time_base().ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                // If the timestamp overflows, the seek if out-of-range.
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        // Range check.
        let (min_ts, max_ts) = self.seek_bounds()?;

        if required_ts < min_ts {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        else if let Some(max_ts) = max_ts {
            if required_ts > max_ts {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        match &mut self.packet_info {
            PacketInfo::FixedAudioPacket {
                bytes_per_packet, frames_per_packet, start_pts, ..
            } => {
                let is_uncompressed = frames_per_packet.get() == 1;

                let frames_per_packet = if is_uncompressed {
                    // Packetization for uncompressed data is performed by chunking the stream into
                    // packets of MAX_FRAMES_PER_PACKET frames each.
                    MAX_FRAMES_PER_PACKET
                }
                else {
                    u64::from(frames_per_packet.get())
                };

                // Calculate the duration from the start PTS. This is equal to the number of frames
                // to the requested PTS. The seek is out-of-range if this is negative. To maintain
                // deterministic packet timestamps, the seek must jump to a packet boundary before
                // the requested seek time, so align down to a packet boundary.
                let dur_from_start = required_ts
                    .duration_from(*start_pts)
                    .and_then(|d| d.align_towards_zero(Duration::from(frames_per_packet)))
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                // Calculate the actual PTS that will be seeked to after aligning down.
                let actual_ts = start_pts
                    .checked_add(dur_from_start)
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                // Calculate the position to seek to. If compressed, there may be more than one
                // frame per packet so the duration from start must be converted from frames to
                // packets by dividing by frames-per-packet. This is lossless since we're aligned
                // to a multiple of frames-per-packet in all cases.
                let seek_pos = self.data_start_pos
                    + u64::from(bytes_per_packet.get()) * dur_from_start.get()
                        / if is_uncompressed { 1 } else { frames_per_packet };

                if self.reader.is_seekable() {
                    self.reader.seek(SeekFrom::Start(seek_pos))?;
                }
                else {
                    let current_pos = self.reader.pos();
                    if seek_pos >= current_pos {
                        self.reader.ignore_bytes(seek_pos - current_pos)?;
                    }
                    else {
                        return seek_error(SeekErrorKind::ForwardOnly);
                    }
                }

                debug!(
                    "seek required_ts: {}, actual_ts: {}, (difference: {})",
                    required_ts,
                    actual_ts,
                    actual_ts.saturating_delta(required_ts),
                );

                Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
            }
            PacketInfo::VariableAudioPacket { packets, current_packet_index, .. } => {
                let Some(seek_packet_index) = find_seek_packet_index(packets, required_ts)
                else {
                    return decode_error("caf: no packets to seek to");
                };

                let seek_packet = &packets[seek_packet_index];

                let seek_pos = self.data_start_pos + seek_packet.data_offset;

                if self.reader.is_seekable() {
                    self.reader.seek(SeekFrom::Start(seek_pos))?;
                }
                else {
                    let current_pos = self.reader.pos();
                    if seek_pos >= current_pos {
                        self.reader.ignore_bytes(seek_pos - current_pos)?;
                    }
                    else {
                        return seek_error(SeekErrorKind::ForwardOnly);
                    }
                }

                *current_packet_index = seek_packet_index;
                let actual_ts = seek_packet.start_frame;

                debug!(
                    "seek required_ts: {}, actual_ts: {}, (difference: {}, packet: {})",
                    required_ts,
                    actual_ts,
                    actual_ts.saturating_delta(required_ts),
                    seek_packet_index,
                );

                Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
            }
            PacketInfo::Unknown => decode_error("caf: missing packet info"),
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

/// Find the index of the packet to seek to for the required timestamp.
///
/// The returned packet is the last packet starting before the required timestamp so that a seek
/// never lands after it. If no packet starts before the required timestamp, then the first packet
/// is returned. Returns `None` if there are no packets.
fn find_seek_packet_index(packets: &[CafPacket], required_ts: Timestamp) -> Option<usize> {
    if packets.is_empty() {
        return None;
    }

    // The start frame of a packet never decreases, therefore the first packet starting at, or
    // after, the required timestamp may be found with a binary search.
    let packet_after_ts = packets.partition_point(|packet| packet.start_frame < required_ts);

    // Seek to the packet preceding it, or to the first packet if the required timestamp precedes
    // all packets.
    Some(packet_after_ts.saturating_sub(1))
}

impl<'s> CafReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut reader = Self {
            reader: mss,
            media_info: Default::default(),
            tracks: vec![],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            data_start_pos: 0,
            data_len: None,
            packet_info: PacketInfo::Unknown,
        };

        reader.check_file_header()?;
        let track = reader.read_chunks()?;

        // TODO: Fix this weird initialization pattern.
        reader.media_info = MediaInfo::from_track(&track);
        reader.tracks.push(track);

        Ok(reader)
    }

    fn time_base(&self) -> Option<TimeBase> {
        self.tracks.first().and_then(|track| track.time_base)
    }

    fn seek_bounds(&self) -> Result<(Timestamp, Option<Timestamp>)> {
        let (dur, delay, padding) = self
            .tracks
            .first()
            .map(|track| (track.num_frames, track.delay.unwrap_or(0), track.padding.unwrap_or(0)))
            .unwrap_or((None, 0, 0));

        // The timeline starts with the priming frames at a negative timestamp, and ends after the
        // padding frames.
        let min_ts = Timestamp::from(-i64::from(delay));
        let max_ts = dur
            .and_then(|dur| Timestamp::ZERO.checked_add(Duration::from(dur)))
            .and_then(|ts| ts.checked_add(Duration::from(padding)));

        Ok((min_ts, max_ts))
    }

    fn check_file_header(&mut self) -> Result<()> {
        let file_type = self.reader.read_quad_bytes()?;
        if file_type != *b"caff" {
            return unsupported_error("caf: missing 'caff' stream marker");
        }

        let file_version = self.reader.read_be_u16()?;
        if file_version != 1 {
            error!("unsupported file version ({file_version})");
            return unsupported_error("caf: unsupported file version");
        }

        // Ignored in CAF v1
        let _file_flags = self.reader.read_be_u16()?;

        Ok(())
    }

    fn read_audio_description_chunk(
        &mut self,
        desc: &AudioDescription,
        codec_params: &mut AudioCodecParameters,
    ) -> Result<()> {
        let codec = desc.codec_id()?;

        codec_params.for_codec(codec).with_sample_rate(desc.sample_rate as u32);

        // Only linear PCM has meaningful sample widths in the audio description. For all other
        // formats the field is 0, and the decoder derives the sample widths itself (A-law and
        // mu-law samples are expanded to 16-bit).
        //
        // PCM samples that do not fill their byte-aligned container (e.g., 12-bit, or 24-bit
        // unpacked in 4 bytes) are high-aligned within it, so the container is decoded as-is and
        // the number of significant bits is the coded (and decoded) sample width.
        if matches!(desc.format_id, AudioDescriptionFormatId::LinearPCM { .. }) {
            codec_params
                .with_bits_per_sample(desc.bits_per_channel)
                .with_bits_per_coded_sample(desc.bits_per_channel);
        }

        // Without a Channel Layout chunk the channel order is undefined, so default to the
        // conventional WAVE channel order for the channel count. This is overridden by the
        // Channel Layout chunk.
        let channels = match desc.channels_per_frame {
            0 => {
                // A channel count of zero should have been rejected by the AudioDescription parser
                unreachable!("Invalid channel count");
            }
            n => match Position::from_wave_channel_count(n) {
                Some(positions) => Channels::Positioned(positions),
                None => match u16::try_from(n) {
                    Ok(n) => Channels::Discrete(n),
                    Err(_) => return unsupported_error("caf: unsupported channel count"),
                },
            },
        };

        codec_params.with_channels(channels);

        // Need to set max_frames_per_packet in all cases in case it is needed (e.g. ADPCM).
        match desc.frames_per_packet {
            // The number of frames per packet is variable, and described by the packet table.
            0 => (),
            // If uncompressed data, chunk the stream into MAX_FRAMES_PER_PACKET blocks.
            1 => {
                codec_params
                    .with_max_frames_per_packet(MAX_FRAMES_PER_PACKET)
                    .with_frames_per_block(MAX_FRAMES_PER_PACKET);
            }
            frames => {
                codec_params
                    .with_max_frames_per_packet(u64::from(frames))
                    .with_frames_per_block(u64::from(frames));
            }
        }

        self.packet_info = if desc.is_variable_packet_format() {
            PacketInfo::VariableAudioPacket {
                packets: Vec::new(),
                current_packet_index: 0,
                end_pts: None,
            }
        }
        else {
            PacketInfo::FixedAudioPacket {
                bytes_per_packet: NonZero::new(desc.bytes_per_packet)
                    .expect("bytes_per_packet is non-zero in this branch"),
                frames_per_packet: NonZero::new(desc.frames_per_packet)
                    .expect("frames_per_packet is non-zero in this branch"),
                start_pts: Timestamp::new(0),
                end_pts: None,
            }
        };

        Ok(())
    }

    fn read_chunks(&mut self) -> Result<Track> {
        use Chunk::*;

        let mut track = Track::new(0);
        let mut codec_params = AudioCodecParameters::new();
        let mut audio_desc = None;
        let mut packet_table = None;
        let mut data_len_unknown = false;
        let mut have_data = false;
        let mut markers = Vec::new();
        let mut regions = Vec::new();
        let mut strings = Vec::new();

        loop {
            match Chunk::read(&mut self.reader, &audio_desc)? {
                Some(AudioDescription(desc)) => {
                    if audio_desc.is_some() {
                        return decode_error("caf: additional Audio Description chunk");
                    }
                    self.read_audio_description_chunk(&desc, &mut codec_params)?;
                    audio_desc = Some(desc);
                }
                Some(AudioData(data)) => {
                    if have_data {
                        return decode_error("caf: additional Audio Data chunk");
                    }

                    have_data = true;
                    self.data_start_pos = data.start_pos;
                    self.data_len = data.data_len;

                    // A data chunk of unknown length (-1) extends to the end of the stream. The
                    // reader is positioned at the start of the audio data, and there can be no
                    // other chunks after it.
                    data_len_unknown = data.data_len.is_none();
                }
                Some(ChannelLayout(layout)) => {
                    let num_channels = audio_desc.as_ref().map_or(0, |d| d.channels_per_frame);

                    match layout.channels(num_channels) {
                        Some(channels) => {
                            if let Some(channels) = decoded_channels(codec_params.codec, channels) {
                                codec_params.channels = Some(channels);
                            }
                        }
                        None => {
                            // Don't error if the layout doesn't correspond directly to a Symphonia
                            // layout, the channels bitmap was set after the audio description was
                            // read to match the number of channels, and that's probably OK.
                            info!("couldn't convert the channel layout into a channel bitmap");
                        }
                    }
                }
                Some(PacketTable(table)) => packet_table = Some(table),
                Some(MagicCookie(data)) => {
                    match codec_params.codec {
                        CODEC_ID_AAC => {
                            // For AAC, the magic cookie is an ES Descriptor. However, the extra
                            // data format for AAC decoders is solely the content of the
                            // decoder-specific information descriptor.
                            let mut reader = BufReader::new(&data);

                            let (desc_tag, desc_len) = read_object_descriptor_header(&mut reader)?;

                            if desc_tag == ClassTag::EsDescriptor {
                                // Parse the ES Descriptor.
                                let desc = ESDescriptor::read(&mut reader, desc_len)?;

                                // Attach the extra data stored in the decoder-specific
                                // configuration.
                                if let Some(info) = desc.dec_config.dec_specific_info {
                                    codec_params.with_extra_data(info.extra_data);
                                }
                            }
                        }
                        CODEC_ID_FLAC => {
                            // For FLAC, the magic cookie is an MP4 `dfLa` atom, but the extra data
                            // format for FLAC decoders is solely the stream information block.
                            match flac_stream_info_from_cookie(&data) {
                                Some(stream_info) => {
                                    codec_params.with_extra_data(stream_info);
                                }
                                None => warn!("flac magic cookie has no stream information"),
                            }
                        }
                        CODEC_ID_OPUS => {
                            // Only an `OpusHead` can be used by the decoder. The contents of other
                            // magic cookies are not specified. See `finalize_opus_extra_data`.
                            if data.starts_with(b"OpusHead") {
                                codec_params.with_extra_data(data);
                            }
                        }
                        _ => {
                            // For all other formats attach the entire magic cookie.
                            codec_params.with_extra_data(data);
                        }
                    }
                }
                Some(Info(entries)) => {
                    let mut builder = MetadataBuilder::new(CAF_METADATA_INFO);
                    let mut tags = Vec::with_capacity(entries.len());

                    for (key, value) in entries {
                        push_tags(&key, &value, &mut tags);
                    }
                    for tag in tags {
                        builder.add_tag(tag);
                    }

                    self.metadata.push(builder.build());
                }
                Some(Id3(rev)) => self.metadata.push(rev),
                Some(Markers(chunk_markers)) => markers.extend(chunk_markers),
                Some(Regions(chunk_regions)) => regions.extend(chunk_regions),
                Some(Strings(chunk_strings)) => strings.extend(chunk_strings),
                Some(Free) | None => {}
            }

            if audio_desc.is_none() {
                error!("missing audio description chunk");
                return decode_error("caf: missing audio description chunk");
            }

            if data_len_unknown {
                break;
            }

            if let Some(byte_len) = self.reader.byte_len() {
                if self.reader.pos() == byte_len {
                    // If we've reached the end of the file, then the Audio Data chunk should have
                    // had a defined size, and we should seek to the start of the audio data.
                    if self.data_len.is_some() {
                        self.reader.seek(SeekFrom::Start(self.data_start_pos))?;
                    }
                    break;
                }
            }
        }

        let Some(desc) = audio_desc
        else {
            return decode_error("caf: missing audio description chunk");
        };

        if !have_data {
            return decode_error("caf: missing audio data chunk");
        }

        // Chapters in the file take precedence over externally provided chapters.
        if let Some(chapters) = build_chapters(desc.sample_rate, &markers, &regions, &strings) {
            self.chapters = Some(chapters);
        }

        // The reader is now positioned at the start of the audio data.
        if codec_params.codec == CODEC_ID_FLAC && codec_params.extra_data.is_none() {
            self.synthesize_flac_stream_info(&desc, &mut codec_params)?;
        }

        let num_frames = self.apply_packet_table(packet_table, &mut track)?;

        if codec_params.codec == CODEC_ID_OPUS {
            finalize_opus_extra_data(&desc, track.delay, &mut codec_params);
        }

        track.with_codec_params(CodecParameters::Audio(codec_params));

        if let Some(num_frames) = num_frames {
            track.with_num_frames(num_frames);
            // Duration equals the number of frames because the timebase is always 1 / sample rate.
            track.with_duration(Duration::from(num_frames));
        }

        Ok(track)
    }

    /// Build the packet layout (timeline, gapless information, and number of frames) from the
    /// packet table, and the length of the audio data.
    ///
    /// The packet table's priming frames are the frames decoded from the start of the stream that
    /// are not part of the audio, and its valid frames are the number of audio frames in the file.
    /// The valid frames, rather than the remainder frames, are authoritative ("must always be
    /// valid" in the CAF specification). Any frames decoded from the data after the valid frames
    /// are padding.
    ///
    /// The packet timeline starts at `-priming` for compressed formats. The decoders trim the
    /// priming and padding frames from the decoded audio when gapless playback is enabled. The
    /// trim durations are supplied by the packets.
    ///
    /// Uncompressed audio is decoded by decoders that do not support trimming. For that, the
    /// priming and remainder frames are instead excluded from the audio data.
    ///
    /// Returns the number of valid frames, if known.
    fn apply_packet_table(
        &mut self,
        table: Option<PacketTable>,
        track: &mut Track,
    ) -> Result<Option<u64>> {
        // Negative priming or remainder frames are meaningless.
        let (priming, remainder, valid_frames) = match &table {
            Some(t) => (
                u64::from(t.priming_frames.max(0).unsigned_abs()),
                u64::from(t.remainder_frames.max(0).unsigned_abs()),
                t.valid_frames as u64,
            ),
            None => (0, 0, 0),
        };

        // The number of valid frames, given the total number of frames decoded from the packets.
        let get_valid_frames = |total_frames: u64| {
            let available = total_frames.saturating_sub(priming);

            if valid_frames > 0 {
                // Do not trust a number of valid frames that exceeds the data.
                valid_frames.min(available)
            }
            else {
                // The number of valid frames is not stated.
                available.saturating_sub(remainder)
            }
        };

        match &mut self.packet_info {
            PacketInfo::FixedAudioPacket {
                bytes_per_packet,
                frames_per_packet,
                start_pts,
                end_pts,
            } => {
                let bytes_per_packet = u64::from(bytes_per_packet.get());
                let frames_per_packet = u64::from(frames_per_packet.get());

                // The total number of frames in the audio data, if the length of the data is known.
                let total_frames =
                    self.data_len.map(|len| (len / bytes_per_packet) * frames_per_packet);

                if frames_per_packet == 1 {
                    // Uncompressed. Exclude the priming and remainder frames from the audio data.
                    let Some(table) = table
                    else {
                        return Ok(total_frames);
                    };

                    let valid = match total_frames {
                        Some(total_frames) => Some(get_valid_frames(total_frames)),
                        None => (table.valid_frames > 0).then_some(valid_frames),
                    };

                    // Skip the priming frames.
                    let skip = priming * bytes_per_packet;
                    let skip = match self.data_len {
                        Some(len) => skip.min(len),
                        None => skip,
                    };

                    if skip > 0 {
                        self.reader.ignore_bytes(skip)?;
                        self.data_start_pos += skip;
                    }

                    self.data_len = match (valid, self.data_len) {
                        (Some(valid), _) => Some(valid * bytes_per_packet),
                        (None, Some(len)) => Some(len - skip),
                        (None, None) => None,
                    };

                    Ok(valid)
                }
                else {
                    // Compressed.
                    let valid = match (&table, total_frames) {
                        (Some(_), Some(total_frames)) => Some(get_valid_frames(total_frames)),
                        (Some(_), None) => (valid_frames > 0).then_some(valid_frames),
                        (None, _) => return Ok(total_frames),
                    };

                    *start_pts = Timestamp::from(-(priming as i64));
                    *end_pts = valid.map(|valid| Timestamp::from(valid as i64));

                    if priming > 0 {
                        track.with_delay(priming.min(u64::from(u32::MAX)) as u32);
                    }

                    if let (Some(total_frames), Some(valid)) = (total_frames, valid) {
                        let padding = total_frames.saturating_sub(priming).saturating_sub(valid);
                        if padding > 0 {
                            track.with_padding(padding.min(u64::from(u32::MAX)) as u32);
                        }
                    }

                    Ok(valid)
                }
            }
            PacketInfo::VariableAudioPacket { packets, end_pts, .. } => {
                let Some(table) = table
                else {
                    error!("missing packet table");
                    return decode_error("caf: missing packet table");
                };

                *packets = table.packets;

                // Some writers terminate the table with an empty packet (no data and no frames),
                // which no decoder can decode.
                packets.retain(|packet| packet.size > 0 || packet.frames.get() > 0);

                // Discard packets that are not entirely within the audio data. The packet sizes
                // are untrusted.
                if let Some(data_len) = self.data_len {
                    let complete = packets.partition_point(|packet| {
                        packet
                            .data_offset
                            .checked_add(packet.size)
                            .is_some_and(|end| end <= data_len)
                    });

                    if complete < packets.len() {
                        warn!(
                            "packet table has {} packets beyond the audio data",
                            packets.len() - complete
                        );
                        packets.truncate(complete);
                    }
                }

                let total_frames: u64 = packets.iter().map(|packet| packet.frames.get()).sum();
                let valid = get_valid_frames(total_frames);

                *end_pts = Some(Timestamp::from(valid as i64));

                if priming > 0 {
                    track.with_delay(priming.min(u64::from(u32::MAX)) as u32);
                }

                let padding = total_frames.saturating_sub(priming).saturating_sub(valid);
                if padding > 0 {
                    track.with_padding(padding.min(u64::from(u32::MAX)) as u32);
                }

                Ok(Some(valid))
            }
            PacketInfo::Unknown => decode_error("caf: missing packet info"),
        }
    }

    /// Build the FLAC stream information block for a FLAC stream without a magic cookie from the
    /// audio description and the first frame header.
    ///
    /// The reader must be at the start of the audio data, and is left there.
    fn synthesize_flac_stream_info(
        &mut self,
        desc: &AudioDescription,
        codec_params: &mut AudioCodecParameters,
    ) -> Result<()> {
        let mut header = [0u8; 4];

        if self.reader.read_buf_exact(&mut header).is_err() {
            return Ok(());
        }
        self.reader.seek_buffered_rel(-(header.len() as isize));

        // The frame header: 14-bit sync code, then the block size and sample rate codes, then the
        // channel assignment, sample size (bits per sample) code.
        let bits_per_sample = match (header[0], header[1] & 0xfe, (header[3] >> 1) & 0x7) {
            (0xff, 0xf8, 1) => 8,
            (0xff, 0xf8, 2) => 12,
            (0xff, 0xf8, 4) => 16,
            (0xff, 0xf8, 5) => 20,
            (0xff, 0xf8, 6) => 24,
            (0xff, 0xf8, 7) => 32,
            // The bits per sample are in the stream information, which is missing.
            _ => desc.bits_per_channel,
        };

        match flac_stream_info(
            desc.sample_rate,
            desc.channels_per_frame,
            bits_per_sample,
            desc.frames_per_packet,
        ) {
            Some(stream_info) => {
                codec_params.with_extra_data(Box::from(stream_info));
            }
            None => warn!("could not determine the flac stream information"),
        }

        Ok(())
    }
}

/// Build the chapters from the markers and regions of a file. The title of a chapter is the string
/// with the ID of its (first) marker. Returns `None` if there are no chapters.
fn build_chapters(
    sample_rate: f64,
    markers: &[Marker],
    regions: &[Region],
    strings: &[(u32, String)],
) -> Option<ChapterGroup> {
    if markers.is_empty() && regions.is_empty() {
        return None;
    }

    // Marker positions are in frames.
    let tb = (sample_rate.fract() == 0.0 && sample_rate >= 1.0 && sample_rate <= u32::MAX as f64)
        .then(|| TimeBase::try_from_recip(sample_rate as u32))
        .flatten()?;

    let time_of = |marker: &Marker| {
        let frame = marker.frame_position;

        if !frame.is_finite() || frame < 0.0 {
            return None;
        }

        // The cast saturates.
        tb.calc_time(Timestamp::new(frame as i64))
    };

    let tags_of = |marker: &Marker| match strings.iter().find(|(id, _)| *id == marker.id) {
        Some((_, name)) if !name.is_empty() => {
            vec![Tag::new_from_parts("NAME", name.clone(), None)]
        }
        _ => vec![],
    };

    let mut chapters = Vec::with_capacity(markers.len() + regions.len());

    for marker in markers {
        if let Some(start_time) = time_of(marker) {
            chapters.push(Chapter {
                start_time,
                end_time: None,
                start_byte: None,
                end_byte: None,
                tags: tags_of(marker),
                visuals: vec![],
            });
        }
    }

    // A region extends from its first marker to its last marker.
    for region in regions {
        let Some(first) = region.markers.first()
        else {
            continue;
        };

        let Some(start_time) = time_of(first)
        else {
            continue;
        };

        let end_time = region
            .markers
            .last()
            .filter(|_| region.markers.len() > 1)
            .and_then(time_of)
            .filter(|end_time| *end_time >= start_time);

        chapters.push(Chapter {
            start_time,
            end_time,
            start_byte: None,
            end_byte: None,
            tags: tags_of(first),
            visuals: vec![],
        });
    }

    if chapters.is_empty() {
        return None;
    }

    chapters.sort_by_key(|chapter| chapter.start_time);

    Some(ChapterGroup {
        items: chapters.into_iter().map(ChapterGroupItem::Chapter).collect(),
        tags: vec![],
        visuals: vec![],
    })
}

/// Get the channels of the decoded audio of a stream with the channel layout `layout`.
///
/// The channel layout is the order of the channels in the decoded audio. PCM and ADPCM decoders
/// decode the channels in the order they are stored. The other decoders define the order of the
/// channels they decode (the canonical order of the positions of the channels). Their channels are
/// the positions of the layout, if all channels have one.
fn decoded_channels(codec: AudioCodecId, layout: Channels) -> Option<Channels> {
    if !matches!(
        codec,
        CODEC_ID_AAC
            | CODEC_ID_ALAC
            | CODEC_ID_FLAC
            | CODEC_ID_OPUS
            | CODEC_ID_MP1
            | CODEC_ID_MP2
            | CODEC_ID_MP3
    ) {
        return Some(layout);
    }

    match layout {
        Channels::Positioned(_) => Some(layout),
        Channels::Custom(labels) => labels
            .iter()
            .try_fold(Position::empty(), |mask, label| match label {
                ChannelLabel::Positioned(position) => Some(mask | *position),
                _ => None,
            })
            .map(Channels::Positioned),
        _ => None,
    }
}

/// Get the stream information block from a FLAC magic cookie.
///
/// The cookie is an MP4 `dfLa` atom: an atom header (4-byte size, `dfLa`), a 4-byte version and
/// flags, and the FLAC metadata blocks, of which the first is the stream information block. A
/// bare stream marker (`fLaC`) followed by metadata blocks, bare metadata blocks, or a bare stream
/// information block are accepted too.
fn flac_stream_info_from_cookie(cookie: &[u8]) -> Option<Box<[u8]>> {
    const STREAM_INFO_LEN: usize = 34;

    // A bare stream information block cannot be told apart from a metadata block header by its
    // content, so it is identified by its length.
    if cookie.len() == STREAM_INFO_LEN {
        return Some(Box::from(cookie));
    }

    let mut blocks = if cookie.get(4..8) == Some(b"dfLa") {
        cookie.get(12..)?
    }
    else if cookie.starts_with(b"dfLa") {
        cookie.get(8..)?
    }
    else if cookie.starts_with(b"fLaC") {
        &cookie[4..]
    }
    else {
        cookie
    };

    // Each metadata block has a 4-byte header: a last block flag, 7-bit block type (0 for stream
    // information), and a 24-bit length.
    while let [flags, len0, len1, len2, rest @ ..] = blocks {
        let len = usize::from(*len0) << 16 | usize::from(*len1) << 8 | usize::from(*len2);
        let block = rest.get(..len)?;

        if flags & 0x7f == 0 {
            return (len == STREAM_INFO_LEN).then(|| Box::from(block));
        }

        if flags & 0x80 != 0 {
            break;
        }

        blocks = &rest[len..];
    }

    None
}

/// Build a FLAC stream information block. The block sizes, frame sizes, number of samples, and
/// MD5 signature are unknown.
fn flac_stream_info(
    sample_rate: f64,
    channels: u32,
    bits_per_sample: u32,
    frames_per_packet: u32,
) -> Option<[u8; 34]> {
    let sample_rate = sample_rate as u64;

    if !(1..=655_350).contains(&sample_rate)
        || !(1..=8).contains(&channels)
        || !(4..=32).contains(&bits_per_sample)
    {
        return None;
    }

    // The maximum block length bounds the decoder's output buffer. It is the maximum a FLAC frame
    // may have, if the packet duration is not known.
    let block_len_max = if frames_per_packet == 0 {
        u16::MAX
    }
    else {
        u16::try_from(frames_per_packet).ok()?.max(16)
    };

    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&16u16.min(block_len_max).to_be_bytes());
    info[2..4].copy_from_slice(&block_len_max.to_be_bytes());
    // Bytes 4..10 are the unknown (0) minimum and maximum frame sizes.
    let packed = (sample_rate << 44)
        | (u64::from(channels - 1) << 41)
        | (u64::from(bits_per_sample - 1) << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    // Bytes 18..34 are the unknown (0) MD5 signature.

    Some(info)
}

/// Opus decoders require an `OpusHead` as extra data. If the file does not have one in its magic
/// cookie, build it for the mono and stereo streams (channel mapping family 0), for which the
/// channel mapping is implied. The number of priming frames is the pre-skip.
fn finalize_opus_extra_data(
    desc: &AudioDescription,
    delay: Option<u32>,
    codec_params: &mut AudioCodecParameters,
) {
    if codec_params.extra_data.is_some() {
        return;
    }

    if !(1..=2).contains(&desc.channels_per_frame) {
        warn!("opus magic cookie is missing, and the channel mapping is unknown");
        return;
    }

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // Version.
    head.push(desc.channels_per_frame as u8);
    head.extend_from_slice(&(delay.unwrap_or(0).min(u32::from(u16::MAX)) as u16).to_le_bytes());
    head.extend_from_slice(&(desc.sample_rate as u32).to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // Output gain.
    head.push(0); // Channel mapping family 0.

    codec_params.with_extra_data(head.into_boxed_slice());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a list of packets each containing 100 frames.
    fn make_packets(count: u64) -> Vec<CafPacket> {
        (0..count)
            .map(|i| CafPacket {
                data_offset: 10 * i,
                start_frame: Timestamp::new(100 * i as i64),
                frames: Duration::new(100),
                size: 10,
            })
            .collect()
    }

    /// An obviously correct, but linear, implementation of `find_seek_packet_index` to test
    /// against.
    fn find_seek_packet_index_linear(
        packets: &[CafPacket],
        required_ts: Timestamp,
    ) -> Option<usize> {
        if packets.is_empty() {
            return None;
        }

        let num_packets_before_ts =
            packets.iter().filter(|packet| packet.start_frame < required_ts).count();

        Some(num_packets_before_ts.saturating_sub(1))
    }

    #[test]
    fn verify_find_seek_packet_index_without_packets() {
        assert_eq!(find_seek_packet_index(&[], Timestamp::new(0)), None);
    }

    #[test]
    fn verify_find_seek_packet_index_with_one_packet() {
        let packets = make_packets(1);

        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(0)), Some(0));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(50)), Some(0));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(1000)), Some(0));
    }

    #[test]
    fn verify_find_seek_packet_index_within_a_packet() {
        let packets = make_packets(5);

        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(50)), Some(0));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(150)), Some(1));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(250)), Some(2));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(450)), Some(4));
    }

    #[test]
    fn verify_find_seek_packet_index_outside_the_packets() {
        let packets = make_packets(5);

        // Before the first packet, the first packet is used.
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(-100)), Some(0));
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(0)), Some(0));

        // After the last packet, the last packet is used.
        assert_eq!(find_seek_packet_index(&packets, Timestamp::new(10000)), Some(4));
    }

    #[test]
    fn verify_find_seek_packet_index_with_zero_length_packets() {
        // Zero length packets yield repeated start frames. The packets are still ordered, so the
        // search is still valid.
        let packets: Vec<CafPacket> = [0, 100, 100, 100, 200]
            .iter()
            .map(|&start_frame| CafPacket {
                data_offset: 0,
                start_frame: Timestamp::new(start_frame),
                frames: Duration::new(0),
                size: 10,
            })
            .collect();

        for ts in -10..300 {
            let ts = Timestamp::new(ts);
            assert_eq!(
                find_seek_packet_index(&packets, ts),
                find_seek_packet_index_linear(&packets, ts),
                "packet index mismatch for {ts}"
            );
        }
    }

    #[test]
    fn verify_find_seek_packet_index_matches_a_linear_search() {
        // Exhaustively verify the binary search returns the same packet as a linear search for
        // every timestamp in, and around, a set of packets.
        for num_packets in 0..8 {
            let packets = make_packets(num_packets);

            for ts in -100..(100 * num_packets as i64 + 100) {
                let ts = Timestamp::new(ts);
                assert_eq!(
                    find_seek_packet_index(&packets, ts),
                    find_seek_packet_index_linear(&packets, ts),
                    "packet index mismatch for {ts} with {num_packets} packets"
                );
            }
        }
    }

    // -- Synthetic file tests ------------------------------------------------------------------

    use std::io::Cursor;

    use symphonia_core::units::Time;

    use symphonia_core::codecs::audio::well_known::{
        CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S8, CODEC_ID_PCM_S16BE,
        CODEC_ID_PCM_S32LE,
    };

    /// Builds a CAF file with an Audio Description chunk, and an Audio Data chunk.
    #[allow(clippy::too_many_arguments)]
    fn make_caf(
        format_id: &[u8; 4],
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        channels: u32,
        bits_per_channel: u32,
        data_chunk_size: i64,
        data: &[u8],
    ) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"caff");
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());

        file.extend_from_slice(b"desc");
        file.extend_from_slice(&32i64.to_be_bytes());
        file.extend_from_slice(&8000.0f64.to_be_bytes());
        file.extend_from_slice(format_id);
        file.extend_from_slice(&format_flags.to_be_bytes());
        file.extend_from_slice(&bytes_per_packet.to_be_bytes());
        file.extend_from_slice(&frames_per_packet.to_be_bytes());
        file.extend_from_slice(&channels.to_be_bytes());
        file.extend_from_slice(&bits_per_channel.to_be_bytes());

        file.extend_from_slice(b"data");
        file.extend_from_slice(&data_chunk_size.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes()); // Edit count.
        file.extend_from_slice(data);
        file
    }

    fn open(file: Vec<u8>) -> Result<CafReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(file)), Default::default());
        CafReader::try_new(mss, FormatOptions::default())
    }

    fn audio_params(reader: &CafReader<'_>) -> AudioCodecParameters {
        reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone()
    }

    #[test]
    fn verify_8bit_pcm_is_supported() {
        // Little-endian flag set, and clear: 8-bit samples have no endianness.
        for flags in [0, 2] {
            let data = (0..200u32).map(|i| i as u8).collect::<Vec<u8>>();
            let mut reader =
                open(make_caf(b"lpcm", flags, 2, 1, 2, 8, 4 + data.len() as i64, &data)).unwrap();

            let params = audio_params(&reader);
            assert_eq!(params.codec, CODEC_ID_PCM_S8);
            assert_eq!(params.bits_per_sample, Some(8));
            assert_eq!(reader.tracks()[0].num_frames, Some(100));

            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(&packet.data[..], &data[..]);
            assert_eq!(packet.dur.get(), 100);
            assert!(reader.next_packet().unwrap().is_none());
        }
    }

    #[test]
    fn verify_alaw_and_ulaw_have_implicit_sample_width() {
        for (id, codec) in [(b"alaw", CODEC_ID_PCM_ALAW), (b"ulaw", CODEC_ID_PCM_MULAW)] {
            let data = [0x55u8; 64];
            let reader = open(make_caf(id, 0, 1, 1, 1, 8, 4 + data.len() as i64, &data)).unwrap();

            let params = audio_params(&reader);
            assert_eq!(params.codec, codec);
            // The PCM decoder expands A-law and mu-law samples to 16-bit and derives the sample
            // widths itself. They must not be set to the 8-bit coded width.
            assert_eq!(params.bits_per_sample, None);
            assert_eq!(params.bits_per_coded_sample, None);
            assert_eq!(reader.tracks()[0].num_frames, Some(64));
        }
    }

    #[test]
    fn verify_unknown_length_data_chunk_ends_with_no_packet() {
        // 16-bit mono. A data chunk size of -1 means the audio data extends to the end of the
        // stream. The stream ends part way through a packet, and a frame.
        let data = vec![0x11u8; 3 * 1152 * 2 + 700 + 1];
        let mut reader = open(make_caf(b"lpcm", 2, 2, 1, 1, 16, -1, &data)).unwrap();

        assert_eq!(reader.tracks()[0].num_frames, None);

        let mut frames = 0;
        let mut pts = 0;

        loop {
            match reader.next_packet() {
                Ok(Some(packet)) => {
                    assert_eq!(packet.pts.get(), pts);
                    assert_eq!(packet.data.len() as u64, packet.dur.get() * 2);
                    pts += packet.dur.get() as i64;
                    frames += packet.dur.get();
                }
                Ok(None) => break,
                Err(err) => panic!("unexpected error at the end of the stream: {err:?}"),
            }
        }

        // Only whole frames are returned.
        assert_eq!(frames, 3 * 1152 + 350);
    }

    // --- Tests using files with arbitrary chunks. ---

    /// A chunk with the type `id`.
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as i64).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn desc_body(
        format_id: &[u8; 4],
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        channels: u32,
        bits_per_channel: u32,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&48000.0f64.to_be_bytes());
        out.extend_from_slice(format_id);
        out.extend_from_slice(&format_flags.to_be_bytes());
        out.extend_from_slice(&bytes_per_packet.to_be_bytes());
        out.extend_from_slice(&frames_per_packet.to_be_bytes());
        out.extend_from_slice(&channels.to_be_bytes());
        out.extend_from_slice(&bits_per_channel.to_be_bytes());
        out
    }

    fn pakt_body(packets: i64, valid: i64, priming: i32, remainder: i32, sizes: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&packets.to_be_bytes());
        out.extend_from_slice(&valid.to_be_bytes());
        out.extend_from_slice(&priming.to_be_bytes());
        out.extend_from_slice(&remainder.to_be_bytes());
        // Every size is below 2^14.
        for &size in sizes {
            if size >= 128 {
                out.push(0x80 | (size >> 7) as u8);
            }
            out.push((size & 0x7f) as u8);
        }
        out
    }

    /// A file with the chunks in the given order. The data chunk is created from `data`.
    fn make_file(chunks: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"caff");
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());
        for chunk in chunks {
            file.extend_from_slice(chunk);
        }
        let mut body = 0u32.to_be_bytes().to_vec();
        body.extend_from_slice(data);
        file.extend_from_slice(&chunk(b"data", &body));
        file
    }

    /// Read all packets, returning (pts, dur, trim start, trim end, size).
    fn read_all(reader: &mut CafReader<'_>) -> Vec<(i64, u64, u64, u64, usize)> {
        let mut out = Vec::new();
        while let Some(packet) = reader.next_packet().unwrap() {
            out.push((
                packet.pts.get(),
                packet.dur.get(),
                packet.trim_start.get(),
                packet.trim_end.get(),
                packet.data.len(),
            ));
        }
        out
    }

    #[test]
    fn verify_variable_packet_priming_and_remainder_are_trimmed() {
        // Five 1024 frame packets: 5120 frames = 1500 priming + 3000 valid + 620 remainder. The
        // packet table follows the data chunk.
        let sizes = [10, 20, 30, 40, 50];
        let data = vec![0u8; 150];
        let mut file = make_file(&[chunk(b"desc", &desc_body(b".mp3", 0, 0, 1024, 2, 0))], &data);
        file.extend_from_slice(&chunk(b"pakt", &pakt_body(5, 3000, 1500, 620, &sizes)));

        let mut reader = open(file).unwrap();
        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(3000));
        assert_eq!(track.start_ts, Timestamp::new(0));
        assert_eq!(track.delay, Some(1500));
        assert_eq!(track.padding, Some(620));

        assert_eq!(
            read_all(&mut reader),
            [
                (-1500, 0, 1024, 0, 10),
                (-476, 548, 476, 0, 20),
                (548, 1024, 0, 0, 30),
                (1572, 1024, 0, 0, 40),
                (2596, 404, 0, 620, 50),
            ]
        );
    }

    #[test]
    fn verify_constant_packet_size_compressed_priming_and_remainder_are_trimmed() {
        // IMA4-like: 34 byte packets of 64 frames. No packet count in the table: the data chunk
        // determines it. 4 packets = 256 frames = 10 priming + 200 valid + 46 remainder.
        let data = vec![0u8; 4 * 34];
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b"ima4", 0, 34, 64, 1, 4)),
                chunk(b"pakt", &pakt_body(0, 200, 10, 46, &[])),
            ],
            &data,
        );

        let mut reader = open(file).unwrap();
        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(200));
        assert_eq!(track.delay, Some(10));
        assert_eq!(track.padding, Some(46));

        assert_eq!(
            read_all(&mut reader),
            [(-10, 54, 10, 0, 34), (54, 64, 0, 0, 34), (118, 64, 0, 0, 34), (182, 18, 0, 46, 34),]
        );
    }

    #[test]
    fn verify_lpcm_priming_and_remainder_window_the_data() {
        // 16-bit mono: 100 frames, 5 priming, 80 valid.
        let data = (0..200u32).map(|i| i as u8).collect::<Vec<u8>>();
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b"lpcm", 2, 2, 1, 1, 16)),
                chunk(b"pakt", &pakt_body(0, 80, 5, 15, &[])),
            ],
            &data,
        );

        let mut reader = open(file).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(80));
        assert_eq!(reader.tracks()[0].start_ts, Timestamp::new(0));

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(&packet.data[..], &data[10..170]);
        assert_eq!(packet.pts.get(), 0);
        assert_eq!(packet.dur.get(), 80);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn verify_empty_trailing_packet_is_dropped() {
        // Variable size and duration packets, terminated by a packet with neither size nor frames.
        let data = vec![0u8; 30];
        let mut pakt = pakt_body(3, 2304, 0, 0, &[]);
        pakt.extend_from_slice(&[10, 0x89, 0x00, 20, 0x89, 0x00, 0, 0]);

        let file = make_file(
            &[chunk(b"desc", &desc_body(b".mp3", 0, 0, 0, 2, 0)), chunk(b"pakt", &pakt)],
            &data,
        );

        let mut reader = open(file).unwrap();
        assert_eq!(read_all(&mut reader).len(), 2);
    }

    #[test]
    fn verify_oversized_packet_is_not_allocated() {
        // The table claims a 2^14 byte packet, but the data chunk is only 10 bytes.
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b".mp3", 0, 0, 1152, 2, 0)),
                chunk(b"pakt", &pakt_body(1, 1152, 0, 0, &[16383])),
            ],
            &[0u8; 10],
        );

        let mut reader = open(file).unwrap();
        assert!(read_all(&mut reader).is_empty());
    }

    #[test]
    fn verify_seek_range_includes_priming_and_remainder() {
        let data = vec![0u8; 150];
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b".mp3", 0, 0, 1024, 2, 0)),
                chunk(b"pakt", &pakt_body(5, 3000, 1500, 620, &[10, 20, 30, 40, 50])),
            ],
            &data,
        );

        let mut reader = open(file).unwrap();

        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(2000), track_id: 0 })
            .unwrap();
        assert_eq!(seeked.actual_ts, Timestamp::new(1572));
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 1572);

        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(-1500), track_id: 0 })
            .unwrap();
        assert_eq!(seeked.actual_ts, Timestamp::new(-1500));

        assert!(
            reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp { ts: Timestamp::new(3621), track_id: 0 }
                )
                .is_err()
        );
    }

    #[test]
    fn verify_unpacked_and_non_byte_aligned_lpcm_use_the_container_width() {
        // 24-bit samples in 4 byte containers (unpacked, high-aligned).
        let reader =
            open(make_file(&[chunk(b"desc", &desc_body(b"lpcm", 2, 8, 1, 2, 24))], &[0u8; 32]))
                .unwrap();
        let params = audio_params(&reader);
        assert_eq!(params.codec, CODEC_ID_PCM_S32LE);
        assert_eq!(params.bits_per_sample, Some(24));
        assert_eq!(reader.tracks()[0].num_frames, Some(4));

        // 12-bit samples in 2 byte containers.
        let reader =
            open(make_file(&[chunk(b"desc", &desc_body(b"lpcm", 0, 2, 1, 1, 12))], &[0u8; 8]))
                .unwrap();
        let params = audio_params(&reader);
        assert_eq!(params.codec, CODEC_ID_PCM_S16BE);

        // The sample width must not exceed the container.
        assert!(
            open(make_file(&[chunk(b"desc", &desc_body(b"lpcm", 2, 2, 1, 1, 24))], &[0u8; 8]))
                .is_err()
        );
    }

    #[test]
    fn verify_channel_layout_tags() {
        // MPEG 5.1 A: L R C LFE Ls Rs is the canonical order.
        let chan = |tag: u32| chunk(b"chan", &[&tag.to_be_bytes()[..], &[0u8; 8]].concat());
        let layout = |tag: u32, channels: u32| {
            let file = make_file(
                &[chunk(b"desc", &desc_body(b"lpcm", 2, 2 * channels, 1, channels, 16)), chan(tag)],
                &[0u8; 24],
            );
            open(file).map(|reader| audio_params(&reader).channels.unwrap())
        };

        let expected = Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::LFE1
            | Position::REAR_LEFT
            | Position::REAR_RIGHT;
        assert_eq!(layout(121 << 16 | 6, 6).unwrap(), Channels::Positioned(expected));

        // AAC 5.1 (C L R Ls Rs LFE) is not in the canonical order.
        match layout(124 << 16 | 6, 6).unwrap() {
            Channels::Custom(labels) => {
                assert_eq!(labels.len(), 6);
                assert_eq!(labels[0], ChannelLabel::Positioned(Position::FRONT_CENTER));
                assert_eq!(labels[5], ChannelLabel::Positioned(Position::LFE1));
            }
            other => panic!("unexpected channels: {other:?}"),
        }

        // The channel count of the tag and of the description disagree: the layout is ignored,
        // and the default one is used.
        assert_eq!(
            layout(124 << 16 | 6, 2).unwrap(),
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        );
    }

    #[test]
    fn verify_compressed_decoders_get_the_positions_of_the_layout() {
        // AAC defines the order of the decoded channels: only the positions are advertised.
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b"aac ", 2, 0, 1024, 6, 0)),
                chunk(b"chan", &[&(124u32 << 16 | 6).to_be_bytes()[..], &[0u8; 8]].concat()),
                chunk(b"pakt", &pakt_body(1, 1024, 0, 0, &[4])),
            ],
            &[0u8; 4],
        );
        let reader = open(file).unwrap();
        assert_eq!(
            audio_params(&reader).channels,
            Some(Channels::Positioned(
                Position::FRONT_LEFT
                    | Position::FRONT_RIGHT
                    | Position::FRONT_CENTER
                    | Position::LFE1
                    | Position::REAR_LEFT
                    | Position::REAR_RIGHT
            ))
        );
    }

    #[test]
    fn verify_flac_stream_info_from_cookie() {
        let mut info = [0u8; 34];
        info[0..2].copy_from_slice(&4096u16.to_be_bytes());
        info[2..4].copy_from_slice(&4096u16.to_be_bytes());

        let mut blocks = vec![0x80, 0, 0, 34];
        blocks.extend_from_slice(&info);

        // MP4 dfLa atom.
        let mut dfla = (blocks.len() as u32 + 12).to_be_bytes().to_vec();
        dfla.extend_from_slice(b"dfLa");
        dfla.extend_from_slice(&[0; 4]);
        dfla.extend_from_slice(&blocks);
        assert_eq!(flac_stream_info_from_cookie(&dfla).as_deref(), Some(&info[..]));

        // Stream marker and blocks, bare blocks, and a bare stream information block.
        let marker = [b"fLaC".as_slice(), &blocks].concat();
        assert_eq!(flac_stream_info_from_cookie(&marker).as_deref(), Some(&info[..]));
        assert_eq!(flac_stream_info_from_cookie(&blocks).as_deref(), Some(&info[..]));
        assert_eq!(flac_stream_info_from_cookie(&info).as_deref(), Some(&info[..]));

        // Truncated.
        assert!(flac_stream_info_from_cookie(&dfla[..dfla.len() - 1]).is_none());
    }

    #[test]
    fn verify_opus_head_is_synthesized_for_mono_and_stereo() {
        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b"opus", 0, 0, 960, 2, 0)),
                chunk(b"pakt", &pakt_body(2, 1920 - 312, 312, 0, &[3, 3])),
            ],
            &[0u8; 6],
        );
        let reader = open(file).unwrap();
        let params = audio_params(&reader);
        let head = params.extra_data.as_ref().unwrap();
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], 2);
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), 312);
        assert_eq!(u32::from_le_bytes([head[12], head[13], head[14], head[15]]), 48000);
    }

    #[test]
    fn verify_ac3_and_eac3_are_mapped() {
        use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};

        for (format_id, codec) in [(b"ac-3", CODEC_ID_AC3), (b"ec-3", CODEC_ID_EAC3)] {
            // Three packets of 1536 frames.
            let file = make_file(
                &[
                    chunk(b"desc", &desc_body(format_id, 0, 0, 1536, 6, 0)),
                    chunk(b"pakt", &pakt_body(3, 3 * 1536, 0, 0, &[10, 20, 30])),
                ],
                &[0u8; 60],
            );
            let mut reader = open(file).unwrap();
            let params = audio_params(&reader);
            assert_eq!(params.codec, codec);
            assert_eq!(params.sample_rate, Some(48000));
            assert_eq!(reader.tracks()[0].num_frames, Some(3 * 1536));

            let packets = read_all(&mut reader);
            assert_eq!(packets.iter().map(|p| p.4).collect::<Vec<_>>(), [10, 20, 30]);
            assert_eq!(packets.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1536, 3072]);
        }
    }

    #[test]
    fn verify_markers_and_regions_become_chapters() {
        fn marker(frame: f64, id: u32) -> Vec<u8> {
            let mut out = 0u32.to_be_bytes().to_vec();
            out.extend_from_slice(&frame.to_be_bytes());
            out.extend_from_slice(&id.to_be_bytes());
            out.extend_from_slice(&[0xff; 8]);
            out.extend_from_slice(&0u32.to_be_bytes());
            out
        }

        let mut strg = 2u32.to_be_bytes().to_vec();
        strg.extend_from_slice(&1u32.to_be_bytes());
        strg.extend_from_slice(&0i64.to_be_bytes());
        strg.extend_from_slice(&2u32.to_be_bytes());
        strg.extend_from_slice(&6i64.to_be_bytes());
        strg.extend_from_slice(b"Intro\0Verse\0");

        // One marker at 1 s, at the 48 kHz sample rate of `desc_body`.
        let mut mark = 0u32.to_be_bytes().to_vec();
        mark.extend_from_slice(&1u32.to_be_bytes());
        mark.extend(marker(48000.0, 1));

        // One region from 0.5 s to 2 s.
        let mut regn = 0u32.to_be_bytes().to_vec();
        regn.extend_from_slice(&1u32.to_be_bytes());
        regn.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
        regn.extend(marker(24000.0, 2));
        regn.extend(marker(96000.0, 2));

        let file = make_file(
            &[
                chunk(b"desc", &desc_body(b"lpcm", 2, 2, 1, 1, 16)),
                chunk(b"strg", &strg),
                chunk(b"mark", &mark),
                chunk(b"regn", &regn),
            ],
            &[0u8; 8],
        );

        let reader = open(file).unwrap();
        let chapters = reader.chapters().unwrap();
        assert_eq!(chapters.items.len(), 2);

        let ChapterGroupItem::Chapter(region) = &chapters.items[0]
        else {
            panic!("expected a chapter");
        };
        assert_eq!(region.start_time, Time::try_from_secs_f64(0.5).unwrap());
        assert_eq!(region.end_time, Some(Time::try_from_secs_f64(2.0).unwrap()));
        assert_eq!(region.tags[0].raw.value.to_string(), "Verse");

        let ChapterGroupItem::Chapter(marker) = &chapters.items[1]
        else {
            panic!("expected a chapter");
        };
        assert_eq!(marker.start_time, Time::try_from_secs_f64(1.0).unwrap());
        assert_eq!(marker.end_time, None);
        assert_eq!(marker.tags[0].raw.value.to_string(), "Intro");
    }
}
