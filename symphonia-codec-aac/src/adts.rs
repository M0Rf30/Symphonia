// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{Error, unsupported_error};
use symphonia_core::support_format;

use symphonia_core::audio::Channels;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::errors::{Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ADTS;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};

use symphonia_common::mpeg::audio::*;

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};

use crate::AacDecoder;
use crate::implicit_sbr::{
    ImplicitExtensions, MAX_IMPLICIT_SBR_CORE_RATE, MAX_PROBE_BLOCKS, build_asc, detect, plain_asc,
    with_explicit_sbr,
};

use log::{debug, info};

/// The `id_syn_ele` of a `program_config_element()`.
const ID_PCE: u32 = 5;

const SAMPLES_PER_AAC_PACKET: Duration = Duration::new(1024);

const ADTS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ADTS,
    short_name: "aac",
    long_name: "Audio Data Transport Stream (native AAC)",
};

/// Audio Data Transport Stream (ADTS) format reader.
///
/// `AdtsReader` implements a demuxer for ADTS (AAC native frames).
///
/// ADTS cannot signal SBR or parametric stereo (HE-AAC): the reader looks for them in the first
/// frames of the stream. If it finds them, the codec parameters describe the decoded output
/// (twice the sample rate of the core codec, and stereo for parametric stereo), the timeline of
/// the track is in decoded frames (the packets have twice the duration of those of the core
/// codec), and the parameters carry an audio specific config that signals the extension, so that
/// the decoder is configured for it up front.
pub struct AdtsReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    first_frame_pos: u64,
    next_packet_ts: Timestamp,
    /// The sample rate of the core codec.
    core_rate: u32,
    /// True if the stream was found to use SBR.
    sbr: bool,
    /// The duration of a packet in decoded frames: 1024 per frame of the core codec, doubled for
    /// SBR.
    packet_dur: Duration,
    /// The plain (without SBR) audio specific config of the stream, if it is AAC-LC with a known
    /// channel layout. It configures the parser that splits frames with multiple blocks.
    plain_asc: Option<Box<[u8]>>,
    /// The parser of the syntax of `raw_data_block()`s, created when a frame has multiple blocks.
    parser: Option<AacDecoder>,
    /// The blocks of the current frame that have not been returned.
    pending: VecDeque<Box<[u8]>>,
}

/// The maximum number of bytes of the stream examined to look for SBR: a limit of the buffering of
/// streams that cannot be rewound.
const MAX_PROBE_LEN: usize = MAX_PROBE_BLOCKS * 8192;

impl<'s> AdtsReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let header = AdtsHeader::read(&mut mss)?;

        // Rewind back to the start of the frame.
        mss.seek_buffered_rev(usize::from(header.header_len()));

        let first_frame_pos = mss.pos();

        // Look for SBR and parametric stereo in the first frames, and, if the channel layout is
        // not predefined, for the program config element that has it.
        let (plain, ext) = probe_stream(&mut mss, &header, first_frame_pos)?;

        // Use the header to populate the codec parameters.
        let mut codec_params = AudioCodecParameters::new();

        codec_params.for_codec(CODEC_ID_AAC).with_sample_rate(header.sample_rate);

        if let Some(channels) = header.channels.clone() {
            codec_params.with_channels(channels);
        }
        else if let Some(asc) =
            plain.as_deref().and_then(|asc| AudioSpecificConfig::read(asc).ok())
        {
            // The channel layout is that of the program config element.
            if let Some(channels) = asc.channels {
                codec_params.with_channels(channels);
            }

            codec_params.with_extra_data(plain.clone().unwrap_or_default());
        }

        // If the stream has SBR, the codec parameters describe the output of the decoder, and
        // the audio specific config tells the decoder.
        let mut ratio = 1;

        if let (true, Some(plain)) = (ext.sbr, plain.as_deref()) {
            let out_rate = header.sample_rate.saturating_mul(2);

            if let Some(extra_data) = with_explicit_sbr(plain, out_rate, ext.ps) {
                if let Ok(asc) = AudioSpecificConfig::read(&extra_data) {
                    info!(
                        "adts: stream has {}, output is {} Hz",
                        if ext.ps { "sbr and parametric stereo" } else { "sbr" },
                        asc.output_sample_rate()
                    );

                    codec_params.with_sample_rate(asc.output_sample_rate());

                    if let Some(channels) = asc.output_channels() {
                        codec_params.with_channels(channels);
                    }

                    if let Some(profile) = get_audio_codec_profile(&asc) {
                        codec_params.with_profile(profile);
                    }

                    codec_params.with_extra_data(extra_data);

                    ratio = u64::from(asc.output_sample_rate() / header.sample_rate.max(1)).max(1);
                }
            }
        }

        let packet_dur = Duration::new(SAMPLES_PER_AAC_PACKET.get() * ratio);

        // Populat the track.
        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        if let Some(num_frames) = approximate_frame_count(&mut mss)? {
            info!("estimating duration from bitrate, may be inaccurate for vbr files");

            // The timeline is in decoded frames, so the duration equals the number of frames
            // because the timebase is always 1 / sample rate.
            let num_frames = num_frames * ratio;
            track.with_num_frames(num_frames);
            track.with_duration(Duration::from(num_frames));
        }

        Ok(AdtsReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            first_frame_pos,
            next_packet_ts: Timestamp::new(0),
            core_rate: header.sample_rate,
            sbr: ext.sbr,
            packet_dur,
            plain_asc: plain,
            parser: None,
            pending: VecDeque::new(),
        })
    }
}

/// Examines the first frames of the stream, which is positioned at the start of the first frame
/// (at `first_frame_pos`) and is left there. Returns the plain (AAC-LC, without SBR) audio specific
/// config of the stream, if the channel layout is known, and the extensions (SBR and parametric
/// stereo) that the stream uses.
///
/// ADTS has no config for the extensions, so they are looked for in the first blocks. The channel
/// layout is a predefined `channel_configuration`, or, if that is 0, the program config element at
/// the start of the first block.
fn probe_stream(
    mss: &mut MediaSourceStream<'_>,
    header: &AdtsHeader,
    first_frame_pos: u64,
) -> Result<(Option<Box<[u8]>>, ImplicitExtensions)> {
    // Only a stream of AAC-LC may be decoded, and have implicit SBR, if its rate can be doubled.
    let can_have_sbr = header.profile == AudioObjectType::Lc
        && header.channels.is_some()
        && header.sample_rate <= MAX_IMPLICIT_SBR_CORE_RATE;

    let needs_pce = header.channel_config == 0 && header.profile == AudioObjectType::Lc;

    if !can_have_sbr && !needs_pce {
        let plain = (header.profile == AudioObjectType::Lc && header.channels.is_some())
            .then(|| plain_asc(header.sample_rate, header.channel_config));

        return Ok((plain, ImplicitExtensions::default()));
    }

    if !mss.is_seekable() {
        mss.ensure_seekback_buffer(MAX_PROBE_LEN);
    }

    let mut blocks = Vec::with_capacity(MAX_PROBE_BLOCKS);
    let mut n_bytes = 0;

    while blocks.len() < MAX_PROBE_BLOCKS && n_bytes < MAX_PROBE_LEN {
        let Ok(frame) = AdtsHeader::read(mss)
        else {
            break;
        };

        let Ok(payload) = mss.read_boxed_slice_exact(usize::from(frame.payload_len()))
        else {
            break;
        };

        n_bytes += usize::from(frame.frame_len);
        blocks.push(payload);
    }

    // Return to the first frame.
    if mss.is_seekable() {
        mss.seek(SeekFrom::Start(first_frame_pos))?;
    }
    else {
        mss.seek_buffered(first_frame_pos);
    }

    let plain: Option<Box<[u8]>> = if needs_pce {
        // The program config element is the first element of a block, ISO/IEC 14496-3 4.5.2.1.
        blocks
            .iter()
            .find_map(|block| {
                let mut bs = BitReaderLtr::new(block);
                (bs.read_bits_leq32(3).ok()? == ID_PCE)
                    .then(|| ProgramConfig::read(&mut bs).ok())?
            })
            .map(|pce| build_asc(2, header.sample_rate, 0, Some(&pce), None))
    }
    else {
        Some(plain_asc(header.sample_rate, header.channel_config))
    };

    let ext = match plain.as_deref() {
        Some(plain) if header.sample_rate <= MAX_IMPLICIT_SBR_CORE_RATE => {
            detect(plain, header.sample_rate, blocks.iter().map(|b| &b[..]))
        }
        _ => ImplicitExtensions::default(),
    };

    Ok((plain, ext))
}

impl Scoreable for AdtsReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // Read the first (assumed) ADTS header.
        let hdr1 = AdtsHeader::read_no_resync(&mut src)?;

        // Since the first header was read successfully, this may be an ADTS audio format. However,
        // if there is enough data left to read the frame body and another frame header, then a
        // higher confidence may be gained. If there is not enough data left, return a partially
        // confident score.
        let payload_len = hdr1.payload_len();

        if src.bytes_available() < u64::from(payload_len + AdtsHeader::SIZE_WITH_CRC) {
            return Ok(Score::Supported(127));
        }

        src.ignore_bytes(u64::from(payload_len))?;

        let _ = AdtsHeader::read_no_resync(&mut src)?;

        Ok(Score::Supported(255))
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct AdtsHeader {
    /// Audio profile.
    profile: AudioObjectType,
    /// Audio channel configuration.
    channels: Option<Channels>,
    /// The `channel_configuration` field.
    channel_config: u32,
    /// The sample rate in Hertz.
    sample_rate: u32,
    /// The length of the ADTS frame in bytes including the sync word, header, and payload. Maximum
    /// value is 8kB.
    frame_len: u16,
    /// An optional CRC.
    crc: Option<u16>,
    /// The number of `raw_data_block()`s in the frame (`number_of_raw_data_blocks_in_frame + 1`).
    raw_data_blocks: u8,
}

impl AdtsHeader {
    /// The size of the an ADTS header CRC.
    pub const CRC_SIZE: u16 = 2;
    /// The size of a ADTS header including the sync word and no a CRC.
    pub const SIZE_NO_CRC: u16 = 7;
    /// The size of a ADTS header including the sync word and CRC.
    pub const SIZE_WITH_CRC: u16 = Self::SIZE_NO_CRC + Self::CRC_SIZE;
    /// The maximum size of the header without the sync word: the error check of a frame of the
    /// maximum 4 raw data blocks has the position of 3 of them, and the CRC.
    const MAX_HEADER_LEN: usize = 5 + 2 * 3 + 2;

    /// Read the body of a header at the current position of the reader.
    fn read_body<B: ReadBytes>(reader: &mut B, has_crc: bool) -> Result<Self> {
        // The fixed and variable header, without the sync word. The number of raw data blocks is
        // in its last two bits, and decides the length of the error check that follows.
        let mut buf = [0; Self::MAX_HEADER_LEN];
        reader.read_buf_exact(&mut buf[..usize::from(Self::SIZE_NO_CRC - 2)])?;

        let raw_data_blocks = (buf[Self::SIZE_NO_CRC as usize - 3] & 0x3) + 1;

        // The error check is the `raw_data_block_position`s of all but the first block (only if
        // there are multiple blocks), and the CRC of the header.
        let check_len = if has_crc {
            Self::CRC_SIZE * (u16::from(raw_data_blocks) - 1) + Self::CRC_SIZE
        }
        else {
            0
        };

        reader.read_buf_exact(
            &mut buf[usize::from(Self::SIZE_NO_CRC - 2)
                ..usize::from(Self::SIZE_NO_CRC - 2 + check_len)],
        )?;

        let len = Self::SIZE_NO_CRC + check_len;

        let mut bs = BitReaderLtr::new(&buf);

        // Profile (audio object type).
        let profile = match get_mpeg4_audio_object_type_by_index(bs.read_bits_leq32(2)? + 1) {
            Some(p) => p,
            None => return decode_error("adts: invalid audio object type"),
        };

        // Sample rate from sample rate index.
        let sample_rate = match get_mpeg4_audio_sample_rate_by_index(bs.read_bits_leq32(4)?) {
            Mpeg4AudioSampleRate::SampleRate(rate) => rate,
            Mpeg4AudioSampleRate::Escape => return decode_error("adts: forbidden sample rate"),
            Mpeg4AudioSampleRate::Invalid => return decode_error("adts: invalid sample rate"),
        };

        // Private bit.
        bs.ignore_bit()?;

        // Channel configuration.
        let channel_config = bs.read_bits_leq32(3)?;

        let channels = match get_mpeg4_audio_channels_by_config_index(channel_config) {
            Mpeg4AudioChannels::Channels(channels) => Some(channels),
            Mpeg4AudioChannels::Escape => None,
            Mpeg4AudioChannels::Invalid => {
                return decode_error("adts: invalid channel configuration");
            }
        };

        // Originality, Home, Copyrighted ID bit, Copyright ID start bits. Only used for encoding.
        bs.ignore_bits(4)?;

        // The frame length = sync word + header + payload.
        let frame_len = bs.read_bits_leq32(13)? as u16;

        // The frame length must be large enough for the header.
        if frame_len < len {
            return decode_error("adts: invalid adts frame length");
        }

        // Buffer fullness.
        let _fullness = bs.read_bits_leq32(11)?;

        // Number of raw data blocks (AAC packets), already read.
        bs.ignore_bits(2)?;

        // The CRC of the header, if provided. It follows the positions of the raw data blocks.
        let crc = if has_crc {
            let pos = usize::from(Self::SIZE_NO_CRC - 2 + check_len - Self::CRC_SIZE);
            Some(u16::from_be_bytes([buf[pos], buf[pos + 1]]))
        }
        else {
            None
        };

        Ok(AdtsHeader {
            profile,
            channels,
            channel_config,
            sample_rate,
            frame_len,
            crc,
            raw_data_blocks,
        })
    }

    /// Returns true if the provided word is a valid sync word.
    #[inline(always)]
    fn is_sync_word(sync: u16) -> bool {
        symphonia_common::mpeg::audio::adts::is_sync_word(sync)
    }

    /// Resync the reader to the next sync word.
    fn sync<B: ReadBytes>(reader: &mut B) -> Result<u16> {
        let sync = read_until_sync(reader, 0, |w| Self::is_sync_word(w as u16))?;

        Ok(sync as u16)
    }

    /// Read a header from the current position of the reader.
    fn read_no_resync<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        let sync = reader.read_be_u16()?;

        if !Self::is_sync_word(sync) {
            return decode_error("adts: invalid frame sync word");
        }

        // "Protection absent" set to 0 if CRC is present.
        Self::read_body(reader, sync & 1 == 0)
    }

    /// Resync the reader if required, and read a header.
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        let sync = AdtsHeader::sync(reader)?;

        // "Protection absent" set to 0 if CRC is present.
        Self::read_body(reader, sync & 1 == 0)
    }

    /// Get the length of the header including the sync word.
    #[inline]
    fn header_len(&self) -> u16 {
        Self::SIZE_NO_CRC
            + if self.crc.is_some() { Self::CRC_SIZE * u16::from(self.raw_data_blocks) } else { 0 }
    }

    /// Get the length of the payload.
    #[inline]
    fn payload_len(&self) -> u16 {
        self.frame_len - self.header_len()
    }
}

impl AdtsReader<'_> {
    /// Read the payload of the frame with the header `header` from the stream, and queue the
    /// `raw_data_block()`s in it.
    ///
    /// A frame has `number_of_raw_data_blocks_in_frame + 1` of them (ISO/IEC 14496-3 Table 1.A.5),
    /// each ending with its `ID_END` element and byte aligned: the end of a block is found by
    /// parsing it. If the frame has a CRC, the payload has a 16 bit check after every block.
    fn read_blocks(&mut self, header: &AdtsHeader) -> Result<()> {
        let payload = self.reader.read_boxed_slice_exact(usize::from(header.payload_len()))?;

        if header.raw_data_blocks == 1 {
            self.pending.push_back(payload);
            return Ok(());
        }

        if self.parser.is_none() {
            let Some(asc) = &self.plain_asc
            else {
                return unsupported_error(
                    "adts: frames with multiple blocks require an aac-lc stream with a known channel layout",
                );
            };

            let mut params = AudioCodecParameters::new();
            params
                .for_codec(CODEC_ID_AAC)
                .with_sample_rate(self.core_rate)
                .with_extra_data(asc.clone());

            self.parser = Some(AacDecoder::try_new_syntax_parser(&params)?);
        }

        let parser = self.parser.as_mut().expect("parser was just created");

        let check_len = if header.crc.is_some() { usize::from(AdtsHeader::CRC_SIZE) } else { 0 };

        let mut rest = &payload[..];

        for i in 0..header.raw_data_blocks {
            let len = if i + 1 == header.raw_data_blocks {
                // The last block is the rest of the frame, without its check.
                match rest.len().checked_sub(check_len) {
                    Some(len) => len,
                    None => return decode_error("adts: truncated raw data block"),
                }
            }
            else {
                parser.measure_raw_data_block(rest)?
            };

            self.pending.push_back(Box::from(&rest[..len]));

            rest = match rest.get(len + check_len..) {
                Some(rest) => rest,
                None => return decode_error("adts: truncated raw data block"),
            };
        }

        Ok(())
    }

    /// Returns true if the stream may use SBR, which ADTS can only signal implicitly.
    fn may_use_sbr(&self) -> bool {
        self.sbr || self.core_rate <= MAX_IMPLICIT_SBR_CORE_RATE
    }
}

impl ProbeableFormat<'_> for AdtsReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AdtsReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ADTS_FORMAT_INFO,
            &["aac"],
            &["audio/aac"],
            &[
                &[0xff, 0xf1], // MPEG 4 without CRC
                &[0xff, 0xf0], // MPEG 4 with CRC
                &[0xff, 0xf9], // MPEG 2 without CRC
                &[0xff, 0xf8], // MPEG 2 with CRC
            ]
        )]
    }
}

impl FormatReader for AdtsReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ADTS_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Read frames until there is a block to return.
        let data = loop {
            if let Some(block) = self.pending.pop_front() {
                break block;
            }

            // Parse the header to get the calculated frame size.
            let header = match AdtsHeader::read(&mut self.reader) {
                Ok(header) => header,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // ADTS streams have no well-defined end, so when no more frames can be read,
                    // consider the stream ended.
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };

            self.read_blocks(&header)?;
        };

        let ts = self.next_packet_ts;

        self.next_packet_ts = match self.next_packet_ts.checked_add(self.packet_dur) {
            Some(ts) => ts,
            None => return Ok(None),
        };

        Ok(Some(Packet::new(0, ts, self.packet_dur, data)))
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

        debug!("seeking to ts={required_ts}");

        // The frame to start decoding from. For a decoder to reproduce a continuous decode at the
        // required timestamp, it must be fed some frames before it (MDCT overlap, SBR state), a
        // choice that depends on the codec features in use. This applies to coarse seeks too: a
        // decoder started at the required timestamp outputs wrong audio, and, with SBR, never
        // recovers the right noise phase.
        let sbr = self.may_use_sbr();
        let packet_dur = self.packet_dur.get();
        let required_frame = u64::try_from(required_ts.get()).unwrap_or(0) / packet_dur;
        let mut start_ts =
            Timestamp::new((aac_seek_start_frame(required_frame, sbr) * packet_dur) as i64);

        // If the frame to start from is before the next packet, attempt to seek to the start of
        // the stream.
        if start_ts < self.next_packet_ts {
            // If the reader is not seekable then only forward seeks are possible.
            if self.reader.is_seekable() {
                let seeked_pos = self.reader.seek(SeekFrom::Start(self.first_frame_pos))?;

                // Since the elementary stream has no timestamp information, the position seeked
                // to must be exactly as requested.
                if seeked_pos != self.first_frame_pos {
                    return seek_error(SeekErrorKind::Unseekable);
                }
            }
            else if required_ts < self.next_packet_ts {
                return seek_error(SeekErrorKind::ForwardOnly);
            }
            else {
                // The stream cannot be rewound to the frame to start decoding from, but the
                // required timestamp can still be reached.
                start_ts = self.next_packet_ts;
            }

            if self.reader.is_seekable() {
                // Successfully seeked to the start of the stream, reset the next packet timestamp.
                self.next_packet_ts = Timestamp::from(0);
                self.pending.clear();
            }
        }

        // Parse frames from the stream until the frame to start from is reached. If the stream
        // cannot be rewound (non-seekable), the frame closest to it that is available is used.
        loop {
            // Skip the blocks of a frame that was partially read.
            if !self.pending.is_empty() {
                match self.next_packet_ts.checked_add(self.packet_dur) {
                    Some(ts) if ts <= start_ts => {
                        self.pending.pop_front();
                        self.next_packet_ts = ts;
                        continue;
                    }
                    _ => break,
                }
            }

            // Parse the next frame header.
            let header = match AdtsHeader::read(&mut self.reader) {
                Ok(header) => header,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // ADTS streams have no well-defined end, so if no more frames can be read then
                    // assume the seek position is out-of-range.
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            // A frame with multiple blocks is split to count them.
            if header.raw_data_blocks > 1 {
                self.read_blocks(&header)?;
                continue;
            }

            let next_packet_ts = match self.next_packet_ts.checked_add(self.packet_dur) {
                Some(ts) if ts <= start_ts => ts,
                // If the next frame's timestamp would exceed the timestamp to start from, or it
                // exceeds the representable range, rewind back to the start of this frame and end
                // the search.
                _ => {
                    self.reader.seek_buffered_rev(usize::from(header.header_len()));
                    break;
                }
            };

            // Ignore the frame body.
            self.reader.ignore_bytes(u64::from(header.payload_len()))?;

            // Increment the timestamp for the next packet.
            self.next_packet_ts = next_packet_ts;
        }

        debug!(
            "seeked to ts={} (delta={})",
            self.next_packet_ts,
            required_ts.saturating_delta(self.next_packet_ts),
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

fn approximate_frame_count(mut source: &mut MediaSourceStream<'_>) -> Result<Option<u64>> {
    let original_pos = source.pos();
    let remaining_len = match source.byte_len() {
        Some(len) => len - original_pos,
        _ => return Ok(None),
    };

    let mut parsed_n_frames = 0u64;
    let mut n_bytes = 0;

    if !source.is_seekable() {
        // The maximum length in bytes of frames to consume from the stream to sample.
        const MAX_LEN: u64 = 16 * 1024;

        source.ensure_seekback_buffer(MAX_LEN as usize);
        let mut scoped_stream = ScopedStream::new(&mut source, MAX_LEN);

        while let Ok(header) = AdtsHeader::read(&mut scoped_stream) {
            if scoped_stream.ignore_bytes(u64::from(header.payload_len())).is_err() {
                break;
            }

            parsed_n_frames += u64::from(header.raw_data_blocks);
            n_bytes += u64::from(header.frame_len);
        }

        let _ = source.seek_buffered(original_pos);
    }
    else {
        // The number of points to sample within the stream.
        const NUM_SAMPLE_POINTS: u64 = 4;
        const NUM_FRAMES_PER_SAMPLE_POINT: u32 = 100;

        let step = remaining_len / NUM_SAMPLE_POINTS;

        // file can be small enough and not have enough NUM_FRAMES_PER_SAMPLE_POINT, but we can
        // still read at least one frame
        if step > 0 {
            for new_pos in (original_pos..(original_pos + remaining_len)).step_by(step as usize) {
                // Skip sample point if previous read exceeded its boundary.
                if new_pos < source.pos() {
                    continue;
                }

                if source.seek(SeekFrom::Start(new_pos)).is_err() {
                    break;
                }

                for _ in 0..NUM_FRAMES_PER_SAMPLE_POINT {
                    let header = match AdtsHeader::read(&mut source) {
                        Ok(header) => header,
                        _ => break,
                    };

                    parsed_n_frames += u64::from(header.raw_data_blocks);
                    n_bytes += u64::from(header.frame_len);

                    // skip frame payload to avoid searching the sync word in the audio data
                    if source.ignore_bytes(header.payload_len() as u64).is_err() {
                        break;
                    }
                }
            }
        }

        let _ = source.seek(SeekFrom::Start(original_pos))?;
    }

    debug!("adts: parsed {n_bytes} of {remaining_len} bytes to approximate duration");

    match parsed_n_frames {
        0 => Ok(None),
        _ => Ok(Some(remaining_len / (n_bytes / parsed_n_frames) * SAMPLES_PER_AAC_PACKET.get())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an ADTS stream of `n` frames with `payload_len` byte payloads. The sample rate index
    /// selects the sample rate (4 = 44.1 kHz, 7 = 22.05 kHz).
    fn adts_stream(n: usize, rate_idx: u8, payload_len: usize) -> Vec<u8> {
        let frame_len = 7 + payload_len;
        let mut data = vec![];

        for i in 0..n {
            // Sync word, MPEG-4, no CRC.
            data.extend_from_slice(&[0xff, 0xf1]);
            // AAC-LC, sample rate, private bit, channel configuration 2 (high bit).
            data.push((1 << 6) | (rate_idx << 2));
            data.push(2 << 6 | ((frame_len >> 11) & 0x3) as u8);
            data.push(((frame_len >> 3) & 0xff) as u8);
            data.push((((frame_len & 0x7) << 5) | 0x1f) as u8);
            // Buffer fullness (low bits) and a single raw data block.
            data.push(0xfc);
            data.extend(std::iter::repeat_n(i as u8, payload_len));
        }

        data
    }

    fn reader(data: Vec<u8>) -> AdtsReader<'static> {
        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        AdtsReader::try_new(mss, Default::default()).expect("adts stream")
    }

    fn seek(reader: &mut AdtsReader<'_>, mode: SeekMode, frame: i64) -> SeekedTo {
        let ts = Timestamp::new(frame * 1024 + 10);
        reader.seek(mode, SeekTo::Timestamp { ts, track_id: 0 }).expect("seek")
    }

    #[test]
    fn seek_starts_a_frame_before_the_target_without_sbr() {
        let mut reader = reader(adts_stream(300, 4, 12));

        for mode in [SeekMode::Accurate, SeekMode::Coarse] {
            let seeked = seek(&mut reader, mode, 150);
            assert_eq!(seeked.actual_ts.get(), 149 * 1024);

            // The reader is positioned at that frame.
            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(packet.pts.get(), 149 * 1024);
            assert_eq!(packet.data[0], 149);
        }

        // Seeks to the start return the first frame.
        assert_eq!(seek(&mut reader, SeekMode::Accurate, 0).actual_ts.get(), 0);
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0);
    }

    #[test]
    fn seek_rewinds_to_a_frame_aligned_start_for_sbr() {
        // 22.05 kHz may be the core rate of an SBR stream.
        let mut reader = reader(adts_stream(400, 7, 12));

        for mode in [SeekMode::Accurate, SeekMode::Coarse] {
            // A decoder is reset to a frame that is a multiple of 16: 41 frames or more before.
            let seeked = seek(&mut reader, mode, 150);
            assert_eq!(seeked.actual_ts.get(), 96 * 1024);
            assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 96);

            // Seeking to a position that has a start frame earlier than the current position
            // must rewind, even if the position is after the start frame.
            let seeked = seek(&mut reader, mode, 130);
            assert_eq!(seeked.actual_ts.get(), 80 * 1024);
            assert_eq!(reader.next_packet().unwrap().unwrap().data[0], 80);

            // And a forward seek, past frames that were not examined.
            let seeked = seek(&mut reader, mode, 390);
            assert_eq!(seeked.actual_ts.get(), 336 * 1024);
            assert_eq!(reader.next_packet().unwrap().unwrap().data[0], (336 % 256) as u8);
        }
    }
}
