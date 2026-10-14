// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::sync::Arc;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{Error, decode_error, seek_error, unsupported_error};
use symphonia_core::errors::{Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_AIFF;
use symphonia_core::io::*;
use symphonia_core::meta::well_known::METADATA_ID_AIFF;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::support_format;

use log::debug;

use crate::common::{
    ByteOrder, ChunksReader, PacketInfo, append_data_params, append_format_params, next_packet,
};
mod chunks;
use chunks::*;

/// Aiff is actually a RIFF stream, with a "FORM" ASCII stream marker.
const AIFF_STREAM_MARKER: [u8; 4] = *b"FORM";
/// A possible RIFF form is "aiff".
const AIFF_RIFF_FORM: [u8; 4] = *b"AIFF";
/// A possible RIFF form is "aifc", using compressed data.
const AIFC_RIFF_FORM: [u8; 4] = *b"AIFC";

const AIFF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_AIFF,
    short_name: "aiff",
    long_name: "Audio Interchange File Format",
};

const AIFF_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: METADATA_ID_AIFF,
    short_name: "aiff",
    long_name: "Audio Interchange File Format",
};

/// Audio Interchange File Format (AIFF) format reader.
///
/// `AiffReader` implements a demuxer for the AIFF container format.
pub struct AiffReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    packet_info: PacketInfo,
    data_start_pos: u64,
    data_end_pos: Option<u64>,
}

impl<'s> AiffReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // An AIFF file is one large RIFF chunk, with the actual meta and audio data contained in
        // nested chunks. Therefore, the file starts with a RIFF chunk header (chunk ID & size).

        // The top-level chunk has the FORM chunk ID. This is also the file marker.
        let marker = mss.read_quad_bytes()?;

        if marker != AIFF_STREAM_MARKER {
            return unsupported_error("aiff: missing aiff riff stream marker");
        }

        // The length of the top-level FORM chunk. Must be atleast 4 bytes.
        let riff_len = mss.read_be_u32()?;

        if riff_len < 4 {
            return decode_error("aiff: invalid riff length");
        }

        // The form type. Only AIFF and AIFC forms are supported.
        let riff_form = mss.read_quad_bytes()?;

        if riff_form != AIFF_RIFF_FORM && riff_form != AIFC_RIFF_FORM {
            return unsupported_error("aiff: riff form is not aiff or aifc");
        }

        let mut riff_chunks =
            ChunksReader::<RiffAiffChunks>::new(Some(riff_len - 4), ByteOrder::BigEndian);

        // Chunks can be read in any order, so collect them to be processed later.
        let mut comm = None;
        let mut ssnd = None;
        let mut mark = None;
        let mut comt = None;
        let mut id3 = None;

        let is_seekable = mss.is_seekable();

        let mut attachments = Vec::new();
        let mut builder = MetadataBuilder::new(AIFF_METADATA_INFO);

        // Scan over all chunks.
        loop {
            let chunk = match riff_chunks.next(&mut mss) {
                Ok(chunk) => chunk,

                // Stop reading without erroring on EOF since `riff_len` can be incorrect.
                Err(Error::IoError(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                    break;
                }

                Err(err) => return Err(err),
            };

            let Some(chunk) = chunk
            else {
                break;
            };

            match chunk {
                RiffAiffChunks::Common(chunk) => {
                    // Only one common chunk is allowed.
                    if comm.is_some() {
                        return decode_error("aiff: multiple common chunks");
                    }

                    comm = match riff_form {
                        AIFF_RIFF_FORM => Some(chunk.parse_aiff(&mut mss)?),
                        AIFC_RIFF_FORM => Some(chunk.parse_aifc(&mut mss)?),
                        _ => unreachable!(),
                    };
                }
                RiffAiffChunks::Sound(chunk) => {
                    // Only one sound data chunk is allowed.
                    if ssnd.is_some() {
                        return decode_error("aiff: multiple sound data chunks");
                    }

                    ssnd = Some(chunk.parse(&mut mss)?);

                    // If the media source is not seekable, then it is not possible to scan for
                    // chunks past the sound data chunk.
                    if !is_seekable {
                        break;
                    }

                    // The length of the sound data chunk must also be known.
                    let ssnd_ref = ssnd.as_ref().expect("ssnd assigned a few lines above");
                    if let Some(len) = ssnd_ref.len {
                        // The file may be truncated within the sound data. Whatever is present can
                        // still be played, so reaching the end of the stream is not an error.
                        match mss.ignore_bytes(u64::from(len)) {
                            Ok(()) => (),
                            Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                            Err(err) => return Err(err.into()),
                        }
                    }
                    else {
                        break;
                    }
                }
                RiffAiffChunks::Marker(chunk) => {
                    // Only one markers chunk is allowed.
                    if mark.is_some() {
                        return decode_error("aiff: multiple markers chunks");
                    }

                    // Saver makers chunk for post-processing.
                    mark = Some(chunk.parse(&mut mss)?)
                }
                RiffAiffChunks::Comments(chunk) => {
                    // Only one comments chunk is allowed.
                    if comt.is_some() {
                        return decode_error("aiff: multiple comments chunks");
                    }

                    // Save comments chunk for post-processing.
                    comt = Some(chunk.parse_and_skip_unread(&mut mss)?);
                }
                RiffAiffChunks::AppSpecific(chunk) => {
                    // Add application-specific data.
                    let appl = chunk.parse_and_skip_unread(&mut mss)?;

                    attachments.push(Attachment::VendorData(VendorDataAttachment {
                        ident: appl.application,
                        data: appl.data,
                    }));
                }
                RiffAiffChunks::Raw(chunk) => {
                    let raw = chunk.parse_and_skip_unread(&mut mss)?;

                    attachments.push(Attachment::VendorData(VendorDataAttachment {
                        ident: raw.ident,
                        data: raw.data,
                    }));
                }
                RiffAiffChunks::Text(chunk) => {
                    // Add tag.
                    let text = chunk.parse_and_skip_unread(&mut mss)?;
                    builder.add_tag(text.tag);
                }
                RiffAiffChunks::Id3(chunk) => id3 = Some(chunk.parse_and_skip_unread(&mut mss)?),
            }
        }

        // The common element is mandatory.
        let comm = comm.ok_or(Error::DecodeError("aiff: missing common element"))?;

        // If the sound data element is not present, then assume an empty one.
        let ssnd = ssnd.unwrap_or_else(|| SoundChunk::empty(mss.pos()));

        // Seek to the start of the sound data.
        if is_seekable {
            mss.seek(SeekFrom::Start(ssnd.data_start_pos))?;
        }

        // Metadata processing.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Process markers and comments.
        let chapters = process_markers(&comm, mark, comt, &mut builder);

        // Add metadata generated from marker, comment, and text chunks.
        // TODO: Don't add if empty.
        metadata.push(builder.build());

        // Add ID3 metadata.
        if let Some(id3) = id3 {
            metadata.push(id3.metadata);
        }

        // The common chunk contains the block_align field and possible additional information
        // to handle packetization and seeking.
        let packet_info = comm.packet_info()?;

        let mut codec_params = AudioCodecParameters::new();
        codec_params
            .with_max_frames_per_packet(packet_info.max_frames_per_packet.get())
            .with_frames_per_block(packet_info.frames_per_block.get());

        // Append common chunk fields to codec parameters.
        append_format_params(&mut codec_params, comm.format_data, comm.sample_rate.get());

        // Create a new track using the collected codec parameters.
        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        // The common chunk is authoritative on the number of sample frames, and the sound data
        // chunk may be larger (e.g., zero padding or trailing bytes). If the declared count is
        // zero, it is unknown (e.g., a streamed file), so the sound data chunk length is used. If
        // the sound data chunk is shorter (e.g., a truncated file), only the data that is present
        // can be decoded.
        //
        // QuickTime and FFmpeg write the number of 34-byte blocks (packets) instead of the number
        // of sample frames for `ima4`. A count equal to the number of blocks present is therefore
        // not a frame count for block-based formats.
        let frames_per_block = packet_info.frames_per_block.get();

        let declared_frames = match u64::from(comm.num_sample_frames) {
            0 => None,
            frames if frames_per_block > 1 => {
                let is_block_count = ssnd
                    .len
                    .is_some_and(|len| u64::from(len) / packet_info.block_size.get() == frames);
                (!is_block_count).then_some(frames)
            }
            frames => Some(frames),
        };

        let data_len = ssnd.len.map(|len| {
            let len = u64::from(len);
            declared_frames
                .and_then(|frames| {
                    frames.div_ceil(frames_per_block).checked_mul(packet_info.block_size.get())
                })
                .map_or(len, |declared_len| len.min(declared_len))
        });

        // Append sound data chunk fields to track.
        if let Some(data_len) = data_len {
            append_data_params(&mut track, data_len, &packet_info);

            // Block-based codecs code whole blocks, so the final block may carry frames past the
            // end of the audio. These are padding.
            let coded_frames = packet_info.get_num_frames_at(data_len);

            if let Some(frames) = declared_frames.filter(|&frames| frames < coded_frames) {
                track.with_num_frames(frames);
                track.with_duration(Duration::from(frames));
                if let Ok(padding) = u32::try_from(coded_frames - frames) {
                    track.with_padding(padding);
                }
            }
        }

        Ok(AiffReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            attachments,
            chapters: chapters.or(opts.external_data.chapters),
            metadata,
            packet_info,
            data_start_pos: ssnd.data_start_pos,
            data_end_pos: data_len.map(|data_len| ssnd.data_start_pos + data_len),
        })
    }
}

fn process_markers(
    comm: &CommonChunk,
    mark: Option<MarkerChunk>,
    comt: Option<CommentsChunk>,
    builder: &mut MetadataBuilder,
) -> Option<ChapterGroup> {
    let mut chapters = Vec::new();
    let mut marker_index = HashMap::new();

    // Process markers chunk.
    if let Some(mark) = mark {
        let tb = TimeBase::from_recip(comm.sample_rate);

        // Create a chapter for each marker.
        chapters.reserve(mark.markers.len());

        for marker in mark.markers {
            // Record the index of the chapter in the chapters vector for the given marker id.
            // Only non-zero positive marker IDs are valid. There should also only be one marker
            // per marker ID.
            if marker.id > 0 && !marker_index.contains_key(&marker.id) {
                marker_index.insert(marker.id, chapters.len());
            }

            // Add the chapter.
            // marker.ts is u32; the result is seconds as i64, so overflow is impossible.
            let start_time = tb
                .calc_time(Timestamp::from(marker.ts))
                .expect("aiff: chapter timestamp overflow is impossible");

            chapters.push(Chapter {
                start_time,
                end_time: None,
                start_byte: None,
                end_byte: None,
                tags: vec![Tag::new_from_parts("NAME", marker.name, None)],
                visuals: vec![],
            });
        }
    }

    // Process comments cjunk.
    if let Some(comt) = comt {
        for comment in comt.comments {
            let value = Arc::new(comment.text);

            let tag =
                Tag::new_from_parts("COMMMENT", value.clone(), Some(StandardTag::Comment(value)));

            if comment.marker_id == 0 {
                // Invalid/unset marker ID, this is a general comment.
                builder.add_tag(tag);
            }
            else if comment.marker_id > 0 {
                // Marker ID is set, this comment belongs to a marker/chapter. Try to get the
                // index of the chapter associated with this marker ID.
                if let Some(idx) = marker_index.get(&comment.marker_id) {
                    // Add tag to chapter.
                    chapters[*idx].tags.push(tag);
                }
            }
        }
    }

    if !chapters.is_empty() {
        Some(ChapterGroup {
            items: chapters.into_iter().map(ChapterGroupItem::Chapter).collect(),
            tags: vec![],
            visuals: vec![],
        })
    }
    else {
        None
    }
}

impl Scoreable for AiffReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // Perform simple scoring by testing that the RIFF stream marker and RIFF form are both
        // valid for AIFF.
        let riff_marker = src.read_quad_bytes()?;
        src.ignore_bytes(4)?;
        let riff_form = src.read_quad_bytes()?;

        if riff_marker != AIFF_STREAM_MARKER {
            return Ok(Score::Unsupported);
        }

        if riff_form != AIFF_RIFF_FORM && riff_form != AIFC_RIFF_FORM {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for AiffReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AiffReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            // AIFF RIFF form
            support_format!(
                AIFF_FORMAT_INFO,
                &["aiff", "aif", "aifc"],
                &["audio/aiff", "audio/x-aiff", " sound/aiff", "audio/x-pn-aiff"],
                &[b"FORM"]
            ),
        ]
    }
}

impl FormatReader for AiffReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &AIFF_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        next_packet(
            &mut self.reader,
            &self.packet_info,
            &self.tracks,
            self.data_start_pos,
            self.data_end_pos.unwrap_or(u64::MAX),
        )
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
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

        let track = &self.tracks[0];

        let required_ts = match to {
            // Frame timestamp given.
            SeekTo::Timestamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the timebase.
            SeekTo::Time { time, .. } => {
                // The timebase is required to calculate the timestamp.
                let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                // If the timestamp overflows, the seek if out-of-range.
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        // Negative timestamps are not allowed.
        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // If the total number of frames in the track is known, verify the desired frame timestamp
        // does not exceed it.
        if let Some(num_frames) = track.num_frames {
            if required_ts.get() as u64 > num_frames {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        debug!("seeking to frame_ts={required_ts}");

        // RIFF is not internally packetized for PCM codecs. Packetization is simulated by trying to
        // read a constant number of samples or blocks every call to next_packet. Therefore, a
        // packet begins wherever the data stream is currently positioned. Since timestamps on
        // packets should be determinstic, instead of seeking to the exact timestamp requested and
        // starting the next packet there, seek to a packet boundary. In this way, packets will have
        // have the same timestamps regardless if the stream was seeked or not.
        let actual_ts = self.packet_info.get_actual_ts(required_ts);

        // Calculate the absolute byte offset of the desired audio frame.
        let seek_pos = self.data_start_pos + self.packet_info.get_data_pos_at(actual_ts);

        // If the reader supports seeking we can seek directly to the frame's offset wherever it may
        // be.
        if self.reader.is_seekable() {
            self.reader.seek(SeekFrom::Start(seek_pos))?;
        }
        // If the reader does not support seeking, we can only emulate forward seeks by consuming
        // bytes. If the reader has to seek backwards, return an error.
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
            "seeked to packet_ts={} (delta={})",
            actual_ts,
            actual_ts.saturating_delta(required_ts)
        );

        Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::codecs::audio::well_known::CODEC_ID_ADPCM_IMA_QT;
    use symphonia_core::formats::FormatOptions;
    use symphonia_core::io::MediaSourceStream;
    use symphonia_core::meta::ChapterGroupItem;

    use super::*;

    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            out.push(0);
        }
        out
    }

    /// An 8 kHz, extended-precision sample rate.
    const RATE_8000: [u8; 10] = [0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0];

    fn comm(channels: u16, frames: u32, bits: u16, compression: Option<&[u8; 4]>) -> Vec<u8> {
        let mut c = channels.to_be_bytes().to_vec();
        c.extend_from_slice(&frames.to_be_bytes());
        c.extend_from_slice(&bits.to_be_bytes());
        c.extend_from_slice(&RATE_8000);
        if let Some(compression) = compression {
            c.extend_from_slice(compression);
            c.extend_from_slice(&[0, 0]); // Empty pstring, plus its pad byte.
        }
        chunk(b"COMM", &c)
    }

    fn ssnd(data: &[u8]) -> Vec<u8> {
        let mut s = vec![0u8; 8];
        s.extend_from_slice(data);
        chunk(b"SSND", &s)
    }

    fn form(kind: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.iter().flatten().copied().collect();
        let mut out = b"FORM".to_vec();
        out.extend_from_slice(&(4 + body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(&body);
        out
    }

    fn open(bytes: Vec<u8>) -> Result<AiffReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        AiffReader::try_new(mss, FormatOptions::default())
    }

    #[test]
    fn comments_with_odd_text_length_are_padded() {
        // Two comments: the first has a 3 byte text (followed by a pad byte).
        let mut comt = 2u16.to_be_bytes().to_vec();
        for (marker, text) in [(0i16, &b"abc"[..]), (0, &b"de"[..])] {
            comt.extend_from_slice(&0u32.to_be_bytes());
            comt.extend_from_slice(&marker.to_be_bytes());
            comt.extend_from_slice(&(text.len() as u16).to_be_bytes());
            comt.extend_from_slice(text);
            if text.len() & 1 == 1 {
                comt.push(0);
            }
        }

        let mut reader =
            open(form(b"AIFF", &[comm(1, 2, 16, None), chunk(b"COMT", &comt), ssnd(&[0; 4])]))
                .unwrap();

        let mut values = Vec::new();
        let mut metadata = reader.metadata();
        loop {
            if let Some(rev) = metadata.current() {
                values.extend(rev.media.tags.iter().map(|t| t.raw.value.to_string()));
            }
            if metadata.is_latest() {
                break;
            }
            metadata.pop();
        }
        assert_eq!(values, ["abc", "de"]);
    }

    #[test]
    fn sound_data_beyond_declared_frames_is_ignored() {
        let reader = open(form(b"AIFF", &[comm(1, 3, 16, None), ssnd(&[0; 12])])).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(3));
    }

    #[test]
    fn truncated_sound_data_is_playable() {
        let mut bytes = form(b"AIFF", &[comm(1, 100, 16, None), ssnd(&[1; 200])]);
        bytes.truncate(bytes.len() - 100);
        let mut reader = open(bytes).unwrap();
        let packet = reader.next_packet().unwrap().expect("packet");
        assert_eq!(packet.dur.get(), 50);
    }

    #[test]
    fn ima4_is_block_based() {
        // 2 channels, 3 blocks of 64 frames; the common chunk declares 150 frames.
        let reader =
            open(form(b"AIFC", &[comm(2, 150, 16, Some(b"ima4")), ssnd(&[0; 3 * 68])])).unwrap();
        let track = &reader.tracks()[0];
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).unwrap();
        assert_eq!(params.codec, CODEC_ID_ADPCM_IMA_QT);
        assert_eq!(params.frames_per_block, Some(64));
        assert_eq!(track.num_frames, Some(150));
        assert_eq!(track.padding, Some(42));
    }

    #[test]
    fn ima4_block_count_in_common_chunk_is_not_a_frame_count() {
        // QuickTime and FFmpeg write the number of blocks for ima4.
        let reader =
            open(form(b"AIFC", &[comm(1, 3, 16, Some(b"ima4")), ssnd(&[0; 3 * 34])])).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(192));
    }

    #[test]
    fn markers_become_chapters() {
        let mut mark = 1u16.to_be_bytes().to_vec();
        mark.extend_from_slice(&1i16.to_be_bytes());
        mark.extend_from_slice(&4000u32.to_be_bytes());
        mark.extend_from_slice(&[3, b'a', b'b', b'c']);
        let reader = open(form(
            b"AIFF",
            &[comm(1, 8000, 16, None), chunk(b"MARK", &mark), ssnd(&[0; 16000])],
        ))
        .unwrap();
        let chapters = reader.chapters().unwrap();
        let ChapterGroupItem::Chapter(chapter) = &chapters.items[0]
        else {
            panic!("expected a chapter");
        };
        assert_eq!(chapter.start_time.as_secs_f64(), 0.5);
    }
}
