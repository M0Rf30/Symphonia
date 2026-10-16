// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IFF sampled sound reader: 8SVX, 16SV, and MAUD.
//!
//! References: the "8SVX" IFF specification (Commodore-Amiga, 1985; "EA IFF 85" for the chunk
//! structure), the 16SV extension (8SVX with 16-bit samples), the MAUD specification (Merlin
//! Amiga audio), and FFmpeg's `libavformat/iff.c` and `libavcodec/8svx.c`, which fix the
//! handling of the chunks and of the compression methods.

use std::io::{self, Seek, SeekFrom};

use symphonia_common::pcm::{MAX_FRAMES_PER_PACKET, PcmLayout, PcmStream, required_ts};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_DPCM_8SVX_EXP, CODEC_ID_DPCM_8SVX_FIB, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW,
    CODEC_ID_PCM_S8_PLANAR, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S16BE_PLANAR, CODEC_ID_PCM_U8,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, limit_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataId, MetadataInfo, MetadataLog, StandardTag, Tag,
};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_from_count, clamp_data_end};

use log::debug;

const FORM: [u8; 4] = *b"FORM";
const FORM_8SVX: [u8; 4] = *b"8SVX";
const FORM_16SV: [u8; 4] = *b"16SV";
const FORM_MAUD: [u8; 4] = *b"MAUD";

/// The limit of the data that is held in memory: the compressed sound of a channel is decoded
/// from one packet, and a source that cannot be seeked has to be read before the channels are.
const MAX_BODY_IN_MEMORY: u64 = 256 * 1024 * 1024;

/// The limit of a text chunk that is read.
const MAX_TEXT_LEN: u32 = 64 * 1024;

const IFF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"IFF ")),
    short_name: "iff",
    long_name: "IFF 8SVX/16SV/MAUD",
};

const IFF_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"IFF ")),
    short_name: "iff",
    long_name: "IFF text chunks",
};

/// The `sCompression` values of the 8SVX header.
const COMPRESSION_NONE: u8 = 0;
const COMPRESSION_FIBONACCI: u8 = 1;
const COMPRESSION_EXPONENTIAL: u8 = 2;

/// Whether the form type is one that the reader supports.
fn is_audio_form(form: &[u8; 4]) -> bool {
    *form == FORM_8SVX || *form == FORM_16SV || *form == FORM_MAUD
}

/// The fields of the header chunk of the sound, and of the other chunks that are of use.
#[derive(Default)]
struct Header {
    rate: Option<u32>,
    channels: Option<u32>,
    /// The 8SVX `sCompression`.
    svx_compression: Option<u8>,
    /// The MAUD sample size in bits, and compression.
    maud_bits: u16,
    maud_compression: u16,
    /// The 8SVX `CHAN` chunk.
    chan: Option<u32>,
    /// The position and size of the sound data.
    body: Option<(u64, u64)>,
    tags: Vec<Tag>,
}

/// Reads a chunk of text into a tag.
fn read_text(
    mss: &mut MediaSourceStream<'_>,
    size: u32,
    key: &str,
    std: fn(String) -> StandardTag,
) -> Result<Option<Tag>> {
    if size > MAX_TEXT_LEN {
        return Ok(None);
    }

    let mut text = vec![0; size as usize];
    mss.read_buf_exact(&mut text)?;

    let end = text.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    let text = String::from_utf8_lossy(&text[..end]).into_owned();

    if text.is_empty() {
        return Ok(None);
    }

    Ok(Some(Tag::new_from_parts(key, text.clone(), Some(std(text)))))
}

/// The sound data of a body that is of channels stored one after the other.
struct PlanarStream {
    /// The position of the sound data in the stream.
    body_pos: u64,
    /// The frames of a channel.
    frames: u64,
    /// The bytes of a sample.
    width: u64,
    channels: u64,
    /// The next frame to read.
    next_frame: u64,
    /// The sound data, if the source cannot be seeked.
    memory: Option<Vec<u8>>,
}

impl PlanarStream {
    fn next_packet(&mut self, reader: &mut MediaSourceStream<'_>) -> Result<Option<Packet>> {
        if self.next_frame >= self.frames {
            return Ok(None);
        }

        let frames = (self.frames - self.next_frame).min(MAX_FRAMES_PER_PACKET);
        let len = (frames * self.width) as usize;

        let mut buf = Vec::new();
        buf.try_reserve_exact(len * self.channels as usize)
            .map_err(|_| Error::IoError(io::Error::from(io::ErrorKind::OutOfMemory)))?;

        // A packet of planar samples holds the frames of the first channel, then the frames of the
        // second, and so on.
        for ch in 0..self.channels {
            let offset = (ch * self.frames + self.next_frame) * self.width;

            if let Some(memory) = &self.memory {
                let offset = offset as usize;
                buf.extend_from_slice(
                    memory
                        .get(offset..offset + len)
                        .ok_or(Error::DecodeError("iff: sound data is out of bounds"))?,
                );
            }
            else {
                let pos = self.body_pos + offset;

                if reader.pos() != pos {
                    reader.seek(SeekFrom::Start(pos))?;
                }

                let start = buf.len();
                buf.resize(start + len, 0);
                reader.read_buf_exact(&mut buf[start..])?;
            }
        }

        let ts = Timestamp::try_from(self.next_frame)
            .map_err(|_| Error::DecodeError("iff: timestamp overflow"))?;

        self.next_frame += frames;

        Ok(Some(Packet::new(0, ts, Duration::new(frames), buf)))
    }
}

/// The sound data of a body that is a single packet.
struct WholeStream {
    body_pos: u64,
    len: u64,
    frames: u64,
    done: bool,
}

impl WholeStream {
    /// Positions the reader at the start of the body.
    fn rewind(&self, reader: &mut MediaSourceStream<'_>) -> Result<()> {
        let pos = reader.pos();

        if pos == self.body_pos {
            Ok(())
        }
        else if reader.is_seekable() {
            reader.seek(SeekFrom::Start(self.body_pos))?;
            Ok(())
        }
        else {
            seek_error(SeekErrorKind::ForwardOnly)
        }
    }

    fn next_packet(&mut self, reader: &mut MediaSourceStream<'_>) -> Result<Option<Packet>> {
        if self.done {
            return Ok(None);
        }

        self.rewind(reader)?;

        let mut buf = vec![0; self.len as usize];
        reader.read_buf_exact(&mut buf)?;

        self.done = true;

        Ok(Some(Packet::new(0, Timestamp::new(0), Duration::new(self.frames), buf)))
    }
}

/// How the sound data is read.
enum Body {
    /// Interleaved samples.
    Pcm(PcmStream),
    /// Samples by channel.
    Planar(PlanarStream),
    /// Compressed data that is one packet.
    Whole(WholeStream),
}

/// IFF sampled sound reader.
///
/// `IffReader` implements a demuxer for IFF `8SVX` (8-bit PCM, Fibonacci-delta and exponential
/// compression), `16SV` (16-bit PCM), and `MAUD` (8 and 16-bit PCM, A-law and mu-law) files. The
/// `NAME`, `AUTH`, `(c) `, `ANNO` and `TEXT` chunks are read as tags.
///
/// The sound data of 8SVX and 16SV files with 2 channels is stored one channel after the other. A
/// source that cannot be seeked is read into memory to interleave the channels. Compressed 8SVX
/// sound is a single packet, so seeking returns to the start of it.
pub struct IffReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    body: Body,
}

impl<'s> IffReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        if mss.read_quad_bytes()? != FORM {
            return unsupported_error("iff: missing FORM chunk");
        }

        let form_size = u64::from(mss.read_be_u32()?);
        let form = mss.read_quad_bytes()?;

        if !is_audio_form(&form) {
            return unsupported_error("iff: unsupported form type");
        }

        // The chunks end at the end of the FORM, or the end of the stream.
        let form_end = clamp_data_end(&mss, 8, Some(form_size)).unwrap_or(8 + form_size);

        let mut header = Header::default();
        let seekable = mss.is_seekable();

        while mss.pos() + 8 <= form_end {
            let id = match mss.read_quad_bytes() {
                Ok(id) => id,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err.into()),
            };
            let size = mss.read_be_u32()?;
            let data_pos = mss.pos();

            // The sound data is the last chunk that is needed if the source cannot be seeked.
            let is_body = id == *b"BODY" || id == *b"MDAT";

            match &id {
                b"VHDR" => {
                    if size < 14 {
                        return decode_error("iff: VHDR chunk is too short");
                    }

                    // oneShotHiSamples, repeatHiSamples, and samplesPerHiCycle.
                    mss.ignore_bytes(12)?;
                    header.rate = Some(u32::from(mss.read_be_u16()?));

                    if size >= 16 {
                        // ctOctave.
                        mss.ignore_bytes(1)?;
                        header.svx_compression = Some(mss.read_u8()?);
                    }

                    header.channels = Some(1);
                }
                b"MHDR" => {
                    if size < 32 {
                        return decode_error("iff: MHDR chunk is too short");
                    }

                    // The number of samples.
                    mss.ignore_bytes(4)?;
                    header.maud_bits = mss.read_be_u16()?;
                    // The sample size after decompression.
                    mss.ignore_bytes(2)?;
                    let num = mss.read_be_u32()?;
                    let den = mss.read_be_u16()?;

                    if den == 0 {
                        return decode_error("iff: invalid MHDR sample rate");
                    }

                    // The channel information.
                    mss.ignore_bytes(2)?;
                    header.rate = Some(num / u32::from(den));
                    header.channels = Some(u32::from(mss.read_be_u16()?));
                    header.maud_compression = mss.read_be_u16()?;
                }
                b"CHAN" if size >= 4 => header.chan = Some(mss.read_be_u32()?),
                b"NAME" => header.tags.extend(read_text(&mut mss, size, "NAME", |t| {
                    StandardTag::TrackTitle(t.into())
                })?),
                b"AUTH" => header
                    .tags
                    .extend(read_text(&mut mss, size, "AUTH", |t| StandardTag::Artist(t.into()))?),
                b"(c) " => header.tags.extend(read_text(&mut mss, size, "(c) ", |t| {
                    StandardTag::Copyright(t.into())
                })?),
                b"ANNO" => header
                    .tags
                    .extend(read_text(&mut mss, size, "ANNO", |t| StandardTag::Comment(t.into()))?),
                b"TEXT" => header
                    .tags
                    .extend(read_text(&mut mss, size, "TEXT", |t| StandardTag::Comment(t.into()))?),
                _ if is_body => {
                    let len = clamp_data_end(&mss, data_pos, Some(u64::from(size)))
                        .map_or(0, |end| end - data_pos);

                    header.body = Some((data_pos, len));
                }
                _ => (),
            }

            if is_body && !seekable {
                break;
            }

            // Chunks are padded to an even size.
            let next = data_pos + u64::from(size) + u64::from(size & 1);

            if next > form_end.max(data_pos) {
                break;
            }

            let pos = mss.pos();

            if pos < next {
                mss.ignore_bytes(next - pos)?;
            }
        }

        let Some((body_pos, body_len)) = header.body
        else {
            return decode_error("iff: missing sound data chunk");
        };

        let Some(rate) = header.rate.filter(|&r| r != 0)
        else {
            return decode_error("iff: missing or invalid sample rate");
        };

        let channels = if form == FORM_MAUD {
            header.channels.unwrap_or(0)
        }
        else {
            // The CHAN chunk gives the sides of the channels: left (2) and right (4) are stereo,
            // and one of them is mono, as FFmpeg does.
            match header.chan {
                Some(chan) if chan >= 6 => 2,
                _ => 1,
            }
        };

        let channel_set = channels_from_count(channels)?;

        let mut params = AudioCodecParameters::new();
        params.with_sample_rate(rate).with_channels(channel_set);

        let mut track = Track::new(0);
        let body;

        if form == FORM_MAUD {
            // Maud sound is interleaved.
            let (codec, layout) = match (header.maud_bits, header.maud_compression) {
                (8, 0) => (CODEC_ID_PCM_U8, PcmLayout::from_sample_bits(8, channels)),
                (16, 0) => (CODEC_ID_PCM_S16BE, PcmLayout::from_sample_bits(16, channels)),
                (8, 2) => (CODEC_ID_PCM_ALAW, PcmLayout::from_sample_bits(8, channels)),
                (8, 3) => (CODEC_ID_PCM_MULAW, PcmLayout::from_sample_bits(8, channels)),
                _ => return unsupported_error("iff: unsupported MAUD compression or bit depth"),
            };

            let layout = layout.ok_or(Error::DecodeError("iff: invalid channel count"))?;

            params.for_codec(codec).with_max_frames_per_packet(layout.max_frames_per_packet());

            if codec == CODEC_ID_PCM_U8 || codec == CODEC_ID_PCM_S16BE {
                params.with_bits_per_coded_sample(u32::from(header.maud_bits));
            }

            let num_frames = layout.num_frames(body_len);
            track.with_num_frames(num_frames).with_duration(Duration::new(num_frames));

            body = Body::Pcm(PcmStream {
                layout,
                start: body_pos,
                end: Some(body_pos + body_len),
                track_id: 0,
            });
        }
        else {
            let codec: AudioCodecId = if form == FORM_16SV {
                CODEC_ID_PCM_S16BE_PLANAR
            }
            else {
                match header.svx_compression.unwrap_or(COMPRESSION_NONE) {
                    COMPRESSION_NONE => CODEC_ID_PCM_S8_PLANAR,
                    COMPRESSION_FIBONACCI => CODEC_ID_DPCM_8SVX_FIB,
                    COMPRESSION_EXPONENTIAL => CODEC_ID_DPCM_8SVX_EXP,
                    _ => return unsupported_error("iff: unknown 8SVX compression method"),
                }
            };

            params.for_codec(codec);

            let ch = u64::from(channels);

            if codec == CODEC_ID_DPCM_8SVX_FIB || codec == CODEC_ID_DPCM_8SVX_EXP {
                // A channel is a pad byte, the initial value, and the codes: two to a byte.
                if body_len < 3 * ch {
                    return decode_error("iff: 8SVX sound data is too short");
                }

                if body_len > MAX_BODY_IN_MEMORY {
                    return limit_error("iff: compressed sound data is too large");
                }

                let frames = 2 * ((body_len / ch).saturating_sub(2));

                params.with_max_frames_per_packet(frames).with_bits_per_coded_sample(4);
                track.with_num_frames(frames).with_duration(Duration::new(frames));

                body = Body::Whole(WholeStream { body_pos, len: body_len, frames, done: false });
            }
            else {
                let width: u64 = if form == FORM_16SV { 2 } else { 1 };
                let frames = body_len / (width * ch);

                params
                    .with_max_frames_per_packet(MAX_FRAMES_PER_PACKET)
                    .with_bits_per_coded_sample(width as u32 * 8);
                track.with_num_frames(frames).with_duration(Duration::new(frames));

                // Interleaving the channels of a source that cannot be seeked takes all the data.
                let memory = if ch > 1 && !seekable {
                    if body_len > MAX_BODY_IN_MEMORY {
                        return limit_error("iff: sound data is too large for the source");
                    }

                    let mut data = vec![0; (frames * width * ch) as usize];
                    mss.read_buf_exact(&mut data)?;
                    Some(data)
                }
                else {
                    None
                };

                body = Body::Planar(PlanarStream {
                    body_pos,
                    frames,
                    width,
                    channels: ch,
                    next_frame: 0,
                    memory,
                });
            }
        }

        track.with_codec_params(CodecParameters::Audio(params)).with_start_ts(Timestamp::new(0));

        let mut metadata = opts.external_data.metadata.take().unwrap_or_default();

        if !header.tags.is_empty() {
            let mut builder = MetadataBuilder::new(IFF_METADATA_INFO);

            for tag in header.tags {
                builder.add_tag(tag);
            }

            metadata.push(builder.build());
        }

        debug!("iff: {rate} Hz, {channels} channels, sound data at {body_pos}+{body_len}");

        // Position the stream at the start of the sound data.
        if seekable {
            mss.seek(SeekFrom::Start(body_pos))?;
        }

        Ok(IffReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata,
            chapters: opts.external_data.chapters,
            body,
        })
    }
}

impl Scoreable for IffReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        if src.read_quad_bytes()? != FORM {
            return Ok(Score::Unsupported);
        }

        let _size = src.read_be_u32()?;

        if is_audio_form(&src.read_quad_bytes()?) {
            Ok(Score::Supported(255))
        }
        else {
            Ok(Score::Unsupported)
        }
    }
}

impl<'s> ProbeableFormat<'s> for IffReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(IffReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            IFF_FORMAT_INFO,
            &["iff", "8svx", "16sv", "svx", "maud"],
            &["audio/x-8svx", "audio/8svx", "audio/x-16sv", "audio/x-maud"],
            &[b"FORM"]
        )]
    }
}

impl FormatReader for IffReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &IFF_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        match &mut self.body {
            Body::Pcm(stream) => stream.next_packet(&mut self.reader),
            Body::Planar(stream) => stream.next_packet(&mut self.reader),
            Body::Whole(stream) => stream.next_packet(&mut self.reader),
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
        let track = &self.tracks[0];

        match &mut self.body {
            Body::Pcm(stream) => stream.seek(&mut self.reader, track, to),
            Body::Planar(stream) => {
                let required_ts = required_ts(track, 0, to)?;

                let actual =
                    (required_ts.get() as u64 / MAX_FRAMES_PER_PACKET) * MAX_FRAMES_PER_PACKET;

                if stream.memory.is_none() && !self.reader.is_seekable() {
                    // The samples of the first channel are read in order.
                    let target = stream.body_pos + actual.min(stream.frames) * stream.width;
                    let pos = self.reader.pos();

                    if target < pos {
                        return seek_error(SeekErrorKind::ForwardOnly);
                    }

                    self.reader.ignore_bytes(target - pos)?;
                }

                stream.next_frame = actual.min(stream.frames);

                let actual_ts = Timestamp::try_from(actual)
                    .map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?;

                Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
            }
            Body::Whole(stream) => {
                let required_ts = required_ts(track, 0, to)?;

                // The packet that is the sound is read again from the start.
                stream.rewind(&mut self.reader)?;
                stream.done = false;

                Ok(SeekedTo { track_id: 0, actual_ts: Timestamp::new(0), required_ts })
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
