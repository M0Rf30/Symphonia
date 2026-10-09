// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{Error, Result, SeekErrorKind};
use symphonia_core::errors::{decode_error, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_WAVE;
use symphonia_core::io::*;
use symphonia_core::meta::well_known::METADATA_ID_WAVE;
use symphonia_core::meta::{Metadata, MetadataInfo, MetadataLog};
use symphonia_core::support_format;

use log::{debug, error, warn};

use crate::common::{
    ByteOrder, ChunksReader, FormatData, PacketInfo, append_data_params, append_format_params,
    next_packet,
};
mod chunks;
mod mpeg;
use chunks::*;

/// WAVE is actually a RIFF stream, with a "RIFF" ASCII stream marker.
const WAVE_STREAM_MARKER: [u8; 4] = *b"RIFF";
/// RF64 is a 64-bit extension of RIFF, with "RF64" as the stream marker.
/// Reference: EBU Tech 3306 - MBWF / RF64: An extended File Format for Audio.
const RF64_STREAM_MARKER: [u8; 4] = *b"RF64";
/// BW64 (ITU-R BS.2088) is the successor to RF64 and is handled identically.
const BW64_STREAM_MARKER: [u8; 4] = *b"BW64";
/// Sony Wave64 files begin with the GUID of the "riff" chunk. The first four bytes of the GUID are
/// the ASCII characters "riff" (lower-case).
const W64_STREAM_MARKER: [u8; 4] = *b"riff";
/// A possible RIFF form is "wave".
const WAVE_RIFF_FORM: [u8; 4] = *b"WAVE";

/// Holds 64-bit size information from the ds64 chunk for RF64 files.
#[derive(Default)]
struct Rf64Sizes {
    /// 64-bit data chunk size (None for standard WAV files).
    data_size: Option<u64>,
    /// 64-bit sample count (None for standard WAV or if not provided).
    sample_count: Option<u64>,
}

const WAVE_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_WAVE,
    short_name: "wave",
    long_name: "Waveform Audio File Format",
};

const WAVE_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: METADATA_ID_WAVE,
    short_name: "wave",
    long_name: "Waveform Audio File Format",
};

/// Waveform Audio File Format (WAV) format reader.
///
/// `WavReader` implements a demuxer for the WAVE container format.
pub struct WavReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    packet_info: PacketInfo,
    data_start_pos: u64,
    data_end_pos: Option<u64>,
    /// True when the data chunk holds an MPEG audio elementary stream (`WAVE_FORMAT_MPEGLAYER3`),
    /// which is packetized frame-by-frame rather than by the block-based `packet_info`.
    is_mpeg: bool,
    /// Running presentation timestamp (in samples) for the next MPEG packet.
    mpeg_ts: u64,
    /// Number of PCM samples per MPEG frame (0 if `is_mpeg` is false or unknown). Used to convert
    /// a seek target directly into a frame index.
    mpeg_samples_per_frame: u64,
    /// Byte offset, relative to the start of the data chunk, of every MPEG frame. MPEG frames are
    /// variable-length, so this index is the only way to seek to an exact frame. It is built
    /// lazily on the first seek (which requires scanning the entire data chunk).
    mpeg_frame_index: Option<Vec<mpeg::FrameIndexEntry>>,
}

impl<'s> WavReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        // A Wave file is one large RIFF chunk, with the actual meta and audio data contained in
        // nested chunks. Therefore, the file starts with a RIFF chunk header (chunk ID & size).

        // The top-level chunk has the RIFF, RF64, BW64, or (Sony Wave64) "riff" chunk ID. This is
        // also the file marker.
        let marker = mss.read_quad_bytes()?;

        let (is_rf64, is_w64) = match marker {
            WAVE_STREAM_MARKER => (false, false),
            // BW64 (ITU-R BS.2088) is the successor to RF64 and uses the same ds64 mechanism.
            RF64_STREAM_MARKER | BW64_STREAM_MARKER => (true, false),
            W64_STREAM_MARKER => (false, true),
            _ => return unsupported_error("wav: missing riff/rf64/bw64/w64 stream marker"),
        };

        let mut w64_chunks = None;

        let riff_data_len = if is_w64 {
            // Sony Wave64 is structurally RIFF, but chunk IDs are 16-byte GUIDs, chunk lengths are
            // 64-bit, and chunks are aligned to 8 bytes.
            read_w64_header(&mut mss)?;
            w64_chunks = Some(W64ChunksReader::new());
            None
        }
        else {
            // The length of the top-level RIFF chunk. Must be atleast 4 bytes.
            // For RF64 files, this is 0xFFFFFFFF and the actual size is in the ds64 chunk.
            let riff_len = mss.read_u32()?;

            if riff_len < 4 && riff_len != u32::MAX {
                return decode_error("wav: invalid riff length");
            }

            // The form type. Only the WAVE form is supported.
            let riff_form = mss.read_quad_bytes()?;

            if riff_form != WAVE_RIFF_FORM {
                error!("riff form is not wave ({})", String::from_utf8_lossy(&riff_form));

                return unsupported_error("wav: riff form is not wave");
            }

            // When ffmpeg encodes wave to stdout the riff (parent) and data (child) chunk lengths
            // are (2^32)-1 since the size is not known ahead of time. For RF64 files, the riff
            // length is also 0xFFFFFFFF.
            if riff_len < u32::MAX { Some(riff_len - 4) } else { None }
        };

        let mut riff_chunks =
            ChunksReader::<RiffWaveChunks>::new(riff_data_len, ByteOrder::LittleEndian);

        let mut codec_params = AudioCodecParameters::new();
        // Seed the log with externally provided metadata (e.g. a leading ID3 tag) so that
        // metadata parsed from the RIFF INFO chunk below is appended to it, not dropped.
        let mut metadata: MetadataLog = opts.external_data.metadata.take().unwrap_or_default();
        let mut packet_info = None;
        let mut fact = None;
        let mut is_mpeg = false;
        let mut mpeg_avg_bytes_per_sec = 0u32;
        let mut rf64_sizes = Rf64Sizes::default();

        loop {
            let chunk = match &mut w64_chunks {
                Some(w64_chunks) => {
                    let chunk = w64_chunks.next(&mut mss)?;

                    // A Wave64 data chunk has a 64-bit length. Treat a length too large for the 32-bit
                    // field like an RF64 data chunk: the true length is carried out-of-band.
                    if let Some((RiffWaveChunks::Data(_), len)) = &chunk {
                        if *len >= u64::from(u32::MAX) {
                            rf64_sizes.data_size = Some(*len);
                        }
                    }

                    chunk.map(|(chunk, _)| chunk)
                }
                None => riff_chunks.next(&mut mss)?,
            };

            // The last chunk should always be a data chunk, if it is not, then the stream is
            // unsupported.
            let Some(chunk) = chunk
            else {
                return unsupported_error("wav: missing data chunk");
            };

            match chunk {
                RiffWaveChunks::Ds64(ds64_parser) => {
                    let ds64 = ds64_parser.parse(&mut mss)?;

                    // ds64 chunk is only meaningful in RF64 files. Ignore in standard WAV.
                    if !is_rf64 {
                        debug!("ignoring ds64 chunk in non-RF64 file");
                        continue;
                    }

                    debug!(
                        "parsed ds64 chunk: data_size={}, sample_count={}",
                        ds64.data_size, ds64.sample_count
                    );

                    rf64_sizes.data_size = Some(ds64.data_size);
                    if ds64.sample_count > 0 {
                        rf64_sizes.sample_count = Some(ds64.sample_count);
                    }
                }
                RiffWaveChunks::Format(fmt) => {
                    let format = fmt.parse(&mut mss)?;

                    // MPEG audio in WAV is framed from the elementary stream, not by fixed blocks.
                    is_mpeg = matches!(format.format_data, FormatData::Mpeg(_));

                    if is_mpeg {
                        mpeg_avg_bytes_per_sec = format.avg_bytes_per_sec;
                    }

                    // The Format chunk contains the block_align field and possible additional
                    // information to handle packetization and seeking.
                    let info = format.packet_info()?;
                    codec_params
                        .with_max_frames_per_packet(info.max_frames_per_packet.get())
                        .with_frames_per_block(info.frames_per_block.get());

                    // Append Format chunk fields to codec parameters.
                    append_format_params(&mut codec_params, format.format_data, format.sample_rate);

                    packet_info = Some(info);
                }
                RiffWaveChunks::Fact(fct) => {
                    fact = Some(fct.parse(&mut mss)?);
                }
                RiffWaveChunks::List(lst) => {
                    let list = lst.parse(&mut mss)?;

                    // Riff Lists can have many different forms, but WavReader only supports Info
                    // lists.
                    match &list.form {
                        b"INFO" => metadata.push(read_info_chunk(&mut mss, list.len)?),
                        _ => list.skip(&mut mss)?,
                    }
                }
                RiffWaveChunks::Id3(id3) => {
                    let chunk_end_pos = mss.pos() + u64::from(id3.len);

                    // A malformed ID3 tag is not fatal. Skip the chunk.
                    match id3.parse(&mut mss) {
                        Ok(id3) => metadata.push(id3.metadata),
                        Err(err) => {
                            warn!("failed to read id3 chunk: {err}");
                            mss.ignore_bytes(chunk_end_pos.saturating_sub(mss.pos()))?;
                        }
                    }
                }
                RiffWaveChunks::Data(dat) => {
                    let data = dat.parse(&mut mss)?;

                    // Record the bounds of the data chunk.
                    let data_start_pos = mss.pos();

                    // Per EBU Tech 3306, ds64 values only replace the corresponding 32-bit
                    // field when that field is set to -1 (0xFFFFFFFF). DataChunk.len is None
                    // when the 32-bit value was 0xFFFFFFFF.
                    let data_len = match data.len {
                        Some(len) => Some(u64::from(len)),
                        None => rf64_sizes.data_size,
                    };

                    let data_end_pos = data_len.and_then(|len| data_start_pos.checked_add(len));

                    // Metadata chunks are commonly written after the data chunk by taggers. If the
                    // source is seekable, look for them now, then return to the start of the data.
                    // Not possible for Wave64 (no support for the chunks), or if the length of the
                    // data chunk is unknown.
                    if let (Some(data_end_pos), false) = (data_end_pos, is_w64) {
                        read_trailing_metadata(
                            &mut mss,
                            data_start_pos,
                            data_end_pos,
                            &mut metadata,
                        )?;
                    }

                    // For MPEG audio the elementary stream is authoritative for the exact codec
                    // (Layer I/II/III) and sample rate, so refine the codec parameters from the
                    // first frame, then rewind so the first packet is read in full. Also derive
                    // the average bytes/second used for duration estimation:
                    // prefer the format chunk's stated average, falling back to the first frame's
                    // own bit-rate if that field is absent or zero.
                    let mut mpeg_sample_rate = 0u32;
                    let mut mpeg_bytes_per_sec = 0u64;
                    let mut mpeg_samples_per_frame = 0u64;

                    if is_mpeg {
                        if let Some((first, _)) =
                            mpeg::read_frame(&mut mss, data_end_pos.unwrap_or(u64::MAX))?
                        {
                            codec_params.for_codec(first.codec).with_sample_rate(first.sample_rate);
                            mpeg_sample_rate = first.sample_rate;
                            mpeg_samples_per_frame = first.samples_per_frame;
                            mpeg_bytes_per_sec = if mpeg_avg_bytes_per_sec > 0 {
                                u64::from(mpeg_avg_bytes_per_sec)
                            }
                            else {
                                u64::from(first.bitrate) / 8
                            };
                        }
                        mss.seek_buffered(data_start_pos);
                    }

                    // Create the track.
                    let mut track = Track::new(0);

                    track.with_codec_params(CodecParameters::Audio(codec_params));

                    let Some(packet_info) = packet_info
                    else {
                        return decode_error("wav: missing format chunk");
                    };

                    // Append Data chunk fields to track (sets num_frames from data length). MPEG
                    // audio frames are variable-length, so the block-based byte-length formula
                    // does not apply; instead prefer, in order: the ds64 sample count (RF64), the
                    // `fact` chunk sample count (ffmpeg always writes one for non-PCM formats and
                    // it accounts for encoder delay/padding), or an estimate derived from the data
                    // length and the average byte rate (falling back to sampling actual frame
                    // sizes if the average byte rate is unknown).
                    if let Some(data_len) = data_len {
                        if is_mpeg {
                            let num_frames = if let Some(sample_count) = rf64_sizes.sample_count {
                                Some(sample_count)
                            }
                            else if let Some(fact) = &fact {
                                Some(u64::from(fact.num_frames))
                            }
                            else if mpeg_bytes_per_sec > 0 && mpeg_sample_rate > 0 {
                                Some(
                                    data_len.saturating_mul(u64::from(mpeg_sample_rate))
                                        / mpeg_bytes_per_sec,
                                )
                            }
                            else {
                                mpeg::estimate_num_frames(
                                    &mut mss,
                                    data_start_pos,
                                    data_end_pos.unwrap_or(u64::MAX),
                                )?
                            };

                            if let Some(num_frames) = num_frames {
                                track.with_num_frames(num_frames);
                                track.with_duration(Duration::from(num_frames));
                            }
                        }
                        else {
                            append_data_params(&mut track, data_len, &packet_info);

                            // For RF64 files, prefer the sample count from ds64 over the computed
                            // value. For standard WAV, prefer fact chunk over computed value.
                            // Applied after append_data_params so the authoritative value wins.
                            if let Some(sample_count) = rf64_sizes.sample_count {
                                track.with_num_frames(sample_count);
                            }
                            else if let Some(fact) = &fact {
                                append_fact_params(&mut track, fact);
                            }
                        }
                    }

                    // Instantiate the reader.
                    return Ok(WavReader {
                        reader: mss,
                        media_info: MediaInfo::from_track(&track),
                        tracks: vec![track],
                        chapters: opts.external_data.chapters,
                        metadata,
                        packet_info,
                        data_start_pos,
                        data_end_pos,
                        is_mpeg,
                        mpeg_ts: 0,
                        mpeg_samples_per_frame,
                        mpeg_frame_index: None,
                    });
                }
            }
        }
    }

    /// Seeks an MPEG-in-WAVE track to the MPEG frame containing `required_ts`.
    ///
    /// MPEG audio frames are variable-length (padding, VBR), so a byte offset can't be calculated
    /// from a timestamp without accumulating error. Instead, on the first seek, the frames of the
    /// whole data chunk are indexed, and the seek is then to the exact byte offset of the frame.
    fn seek_mpeg(&mut self, required_ts: Timestamp) -> Result<SeekedTo> {
        if self.mpeg_samples_per_frame == 0 || !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        debug!("seeking mpeg-in-wave to ts={required_ts}");

        if self.mpeg_frame_index.is_none() {
            self.reader.seek(SeekFrom::Start(self.data_start_pos))?;

            let index = mpeg::scan_frame_offsets(
                &mut self.reader,
                self.data_start_pos,
                self.data_end_pos.unwrap_or(u64::MAX),
            )?;

            self.mpeg_frame_index = Some(index);
        }

        let index = self.mpeg_frame_index.as_deref().unwrap_or_default();

        // Frames are all the same codec so all have the same number of samples.
        let frame_index = (required_ts.get() as u64) / self.mpeg_samples_per_frame;

        let Some(frame_index) = usize::try_from(frame_index).ok().filter(|&i| i < index.len())
        else {
            return seek_error(SeekErrorKind::OutOfRange);
        };

        // Start decoding some frames before the target, and the frames its bit reservoir
        // references, so that the output of the target frame is identical to a continuous decode.
        let start_index = mpeg::preroll_start(index, frame_index);

        self.reader.seek(SeekFrom::Start(self.data_start_pos + index[start_index].offset))?;

        let aligned_ts = (start_index as u64) * self.mpeg_samples_per_frame;

        let actual_ts = Timestamp::try_from(aligned_ts)
            .map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?;

        self.mpeg_ts = aligned_ts;

        debug!(
            "seeked mpeg-in-wave to ts={} (delta={})",
            actual_ts,
            actual_ts.saturating_delta(required_ts)
        );

        Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
    }
}

impl Scoreable for WavReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // Perform simple scoring by testing that the RIFF/RF64/BW64 stream marker and RIFF form
        // are both valid for WAVE.
        let marker = src.read_quad_bytes()?;

        // A Wave64 file starts with the "riff" chunk GUID, and the "wave" chunk GUID follows the
        // 64-bit length.
        if marker == W64_STREAM_MARKER {
            let mut guid_tail = [0u8; 12];
            src.read_buf_exact(&mut guid_tail)?;
            src.ignore_bytes(8)?;
            let mut form = [0u8; 4];
            src.read_buf_exact(&mut form)?;

            return if guid_tail == W64_RIFF_GUID_TAIL && form == *b"wave" {
                Ok(Score::Supported(255))
            }
            else {
                Ok(Score::Unsupported)
            };
        }

        src.ignore_bytes(4)?;
        let riff_form = src.read_quad_bytes()?;

        let is_valid_marker = marker == WAVE_STREAM_MARKER
            || marker == RF64_STREAM_MARKER
            || marker == BW64_STREAM_MARKER;

        if !is_valid_marker || riff_form != WAVE_RIFF_FORM {
            return Ok(Score::Unsupported);
        }

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for WavReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(WavReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            // WAVE RIFF form
            support_format!(
                WAVE_FORMAT_INFO,
                &["wav", "wave"],
                &["audio/vnd.wave", "audio/x-wav", "audio/wav", "audio/wave"],
                &[b"RIFF"]
            ),
            // RF64 extended WAVE format (64-bit extension for files > 4GB)
            support_format!(
                WAVE_FORMAT_INFO,
                &["wav", "wave", "rf64"],
                &["audio/vnd.wave", "audio/x-wav", "audio/wav", "audio/wave"],
                &[b"RF64"]
            ),
            // BW64 (ITU-R BS.2088), the successor to RF64
            support_format!(
                WAVE_FORMAT_INFO,
                &["wav", "wave", "bw64"],
                &["audio/vnd.wave", "audio/x-wav", "audio/wav", "audio/wave"],
                &[b"BW64"]
            ),
            // Sony Wave64 (the GUID of the "riff" chunk)
            support_format!(
                WAVE_FORMAT_INFO,
                &["w64"],
                &["audio/x-w64", "audio/x-wav", "audio/wav"],
                &[b"riff"]
            ),
        ]
    }
}

impl FormatReader for WavReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &WAVE_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // MPEG audio in WAV is framed from the elementary stream, one MPEG frame per packet.
        if self.is_mpeg {
            return mpeg::next_packet(
                &mut self.reader,
                self.data_end_pos.unwrap_or(u64::MAX),
                &mut self.mpeg_ts,
            );
        }

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
            // Time value given, calculate frame timestamp using the time base.
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

        // MPEG-in-WAVE uses a variable-length elementary stream and is seeked by resynchronising
        // to a frame header, rather than the fixed block-size arithmetic used below for PCM/ADPCM.
        if self.is_mpeg {
            return self.seek_mpeg(required_ts);
        }

        debug!("seeking to frame_ts={required_ts}");

        // WAVE is not internally packetized for PCM codecs. Packetization is simulated by trying to
        // read a constant number of samples or blocks every call to next_packet. Therefore, a
        // packet begins wherever the data stream is currently positioned. Since timestamps on
        // packets should be determinstic, instead of seeking to the exact timestamp requested and
        // starting the next packet there, seek to a packet boundary. In this way, packets will have
        // the same timestamps regardless if the stream was seeked or not.
        let actual_ts = self.packet_info.get_actual_ts(required_ts);

        // Calculate the absolute byte offset of the block containing the desired audio frame. The
        // offset is in whole blocks (not frames), which differ for block-based codecs (ADPCM).
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

    use symphonia_core::codecs::audio::well_known::CODEC_ID_MP3;
    use symphonia_core::formats::FormatReader;
    use symphonia_core::io::ReadOnlySource;
    use symphonia_core::units::Time;

    use super::*;

    /// Creates a minimal valid RF64 file in memory.
    fn create_rf64_test_file(data_size: u64, sample_count: u64, pcm_data: &[u8]) -> Vec<u8> {
        let mut file = Vec::new();

        // RF64 header
        file.extend_from_slice(b"RF64");
        file.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // placeholder size
        file.extend_from_slice(b"WAVE");

        // ds64 chunk (28 bytes)
        file.extend_from_slice(b"ds64");
        file.extend_from_slice(&28u32.to_le_bytes()); // chunk size
        let riff_size: u64 = 4 + 8 + 28 + 8 + 16 + 8 + data_size; // WAVE + ds64 + fmt + data
        file.extend_from_slice(&riff_size.to_le_bytes()); // riffSize64
        file.extend_from_slice(&data_size.to_le_bytes()); // dataSize64
        file.extend_from_slice(&sample_count.to_le_bytes()); // sampleCount64
        file.extend_from_slice(&0u32.to_le_bytes()); // tableLength

        // fmt chunk (16 bytes, PCM format)
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        file.extend_from_slice(&1u16.to_le_bytes()); // channels = 1
        file.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        file.extend_from_slice(&88200u32.to_le_bytes()); // byte rate
        file.extend_from_slice(&2u16.to_le_bytes()); // block align
        file.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        // data chunk
        file.extend_from_slice(b"data");
        let chunk_size = if data_size > u32::MAX as u64 { 0xFFFFFFFF } else { data_size as u32 };
        file.extend_from_slice(&chunk_size.to_le_bytes());
        file.extend_from_slice(pcm_data);

        file
    }

    /// Creates a minimal valid standard WAV file in memory.
    fn create_wav_test_file(pcm_data: &[u8]) -> Vec<u8> {
        let mut file = Vec::new();
        let data_len = pcm_data.len() as u32;

        // RIFF header
        file.extend_from_slice(b"RIFF");
        let total_size = 4 + 8 + 16 + 8 + data_len; // WAVE + fmt chunk + data chunk
        file.extend_from_slice(&total_size.to_le_bytes());
        file.extend_from_slice(b"WAVE");

        // fmt chunk
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes()); // PCM
        file.extend_from_slice(&1u16.to_le_bytes()); // mono
        file.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        file.extend_from_slice(&88200u32.to_le_bytes()); // byte rate
        file.extend_from_slice(&2u16.to_le_bytes()); // block align
        file.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        // data chunk
        file.extend_from_slice(b"data");
        file.extend_from_slice(&data_len.to_le_bytes());
        file.extend_from_slice(pcm_data);

        file
    }

    #[test]
    fn test_rf64_small_file() {
        let pcm_data = vec![0u8; 1000]; // 500 samples at 16-bit mono
        let rf64_file = create_rf64_test_file(1000, 500, &pcm_data);

        let source = ReadOnlySource::new(Cursor::new(rf64_file));
        let mss = MediaSourceStream::new(Box::new(source), Default::default());

        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        assert_eq!(reader.tracks.len(), 1);
        assert_eq!(reader.tracks[0].num_frames, Some(500));
    }

    #[test]
    fn test_rf64_large_data_size() {
        let pcm_data = vec![0u8; 100];
        let large_data_size: u64 = 5_000_000_000; // 5GB
        let sample_count = large_data_size / 2; // 16-bit samples
        let rf64_file = create_rf64_test_file(large_data_size, sample_count, &pcm_data);

        let source = ReadOnlySource::new(Cursor::new(rf64_file));
        let mss = MediaSourceStream::new(Box::new(source), Default::default());

        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        assert_eq!(reader.tracks.len(), 1);
        assert_eq!(reader.tracks[0].num_frames, Some(sample_count));
        // data_end_pos should use 64-bit size
        let data_end = reader.data_end_pos.unwrap();
        assert_eq!(data_end - reader.data_start_pos, large_data_size);
    }

    #[test]
    fn test_standard_wav_unchanged() {
        let pcm_data = vec![0u8; 100];
        let wav_file = create_wav_test_file(&pcm_data);

        let source = ReadOnlySource::new(Cursor::new(wav_file));
        let mss = MediaSourceStream::new(Box::new(source), Default::default());

        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        assert_eq!(reader.tracks.len(), 1);
        let data_end = reader.data_end_pos.unwrap();
        assert_eq!(data_end - reader.data_start_pos, 100);
    }

    #[test]
    fn test_rf64_missing_ds64_uses_fallback() {
        // RF64 file without ds64 chunk should still work using 32-bit sizes.
        let mut file = Vec::new();

        file.extend_from_slice(b"RF64");
        let total_size = 4 + 8 + 16 + 8 + 100;
        file.extend_from_slice(&(total_size as u32).to_le_bytes());
        file.extend_from_slice(b"WAVE");

        // fmt chunk
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes()); // PCM
        file.extend_from_slice(&1u16.to_le_bytes()); // mono
        file.extend_from_slice(&44100u32.to_le_bytes());
        file.extend_from_slice(&88200u32.to_le_bytes());
        file.extend_from_slice(&2u16.to_le_bytes());
        file.extend_from_slice(&16u16.to_le_bytes());

        // data chunk
        file.extend_from_slice(b"data");
        file.extend_from_slice(&100u32.to_le_bytes());
        file.extend_from_slice(&vec![0u8; 100]);

        let source = ReadOnlySource::new(Cursor::new(file));
        let mss = MediaSourceStream::new(Box::new(source), Default::default());

        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();
        let data_end = reader.data_end_pos.unwrap();
        assert_eq!(data_end - reader.data_start_pos, 100);
    }

    #[test]
    fn test_ds64_ignored_in_standard_wav() {
        // Standard RIFF/WAV with a ds64 chunk should ignore it.
        let mut file = Vec::new();

        file.extend_from_slice(b"RIFF");
        let total_size = 4 + 8 + 28 + 8 + 16 + 8 + 100; // includes ds64
        file.extend_from_slice(&(total_size as u32).to_le_bytes());
        file.extend_from_slice(b"WAVE");

        // ds64 chunk (should be ignored in standard WAV)
        file.extend_from_slice(b"ds64");
        file.extend_from_slice(&28u32.to_le_bytes());
        file.extend_from_slice(&999999999u64.to_le_bytes()); // fake riff size
        file.extend_from_slice(&888888888u64.to_le_bytes()); // fake data size (should NOT be used)
        file.extend_from_slice(&777777777u64.to_le_bytes()); // fake sample count
        file.extend_from_slice(&0u32.to_le_bytes());

        // fmt chunk
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&44100u32.to_le_bytes());
        file.extend_from_slice(&88200u32.to_le_bytes());
        file.extend_from_slice(&2u16.to_le_bytes());
        file.extend_from_slice(&16u16.to_le_bytes());

        // data chunk
        file.extend_from_slice(b"data");
        file.extend_from_slice(&100u32.to_le_bytes());
        file.extend_from_slice(&vec![0u8; 100]);

        let source = ReadOnlySource::new(Cursor::new(file));
        let mss = MediaSourceStream::new(Box::new(source), Default::default());

        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        // Should use 32-bit size from data chunk, NOT 64-bit from ds64
        let data_end = reader.data_end_pos.unwrap();
        assert_eq!(data_end - reader.data_start_pos, 100);
    }

    /// A single MPEG-1 Layer III frame: 128 kbps, 44.1 kHz, stereo (417 bytes). The body is
    /// zeroed; only framing is under test, not decoding.
    fn mp3_frame() -> Vec<u8> {
        let mut frame = vec![0u8; 417];
        frame[0..4].copy_from_slice(&0xFFFB_9000u32.to_be_bytes());
        frame
    }

    /// Wrap concatenated MPEG audio frames in a minimal RIFF/WAVE container whose `fmt ` chunk
    /// declares `WAVE_FORMAT_MPEGLAYER3` (0x0055), with a configurable `nAvgBytesPerSec` and an
    /// optional `fact` chunk sample count.
    fn riff_mpeglayer3_ex(
        frames: &[Vec<u8>],
        avg_bytes_per_sec: u32,
        fact_num_frames: Option<u32>,
    ) -> Vec<u8> {
        let data: Vec<u8> = frames.iter().flatten().copied().collect();

        // MPEGLAYER3WAVEFORMAT: 16-byte WAVEFORMATEX base + 2-byte cbSize + 12-byte extension.
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&0x0055u16.to_le_bytes()); // wFormatTag
        fmt.extend_from_slice(&2u16.to_le_bytes()); // nChannels
        fmt.extend_from_slice(&44_100u32.to_le_bytes()); // nSamplesPerSec
        fmt.extend_from_slice(&avg_bytes_per_sec.to_le_bytes()); // nAvgBytesPerSec
        fmt.extend_from_slice(&1u16.to_le_bytes()); // nBlockAlign
        fmt.extend_from_slice(&0u16.to_le_bytes()); // wBitsPerSample
        fmt.extend_from_slice(&12u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&1u16.to_le_bytes()); // wID (MPEG Layer 3)
        fmt.extend_from_slice(&0u32.to_le_bytes()); // fdwFlags
        fmt.extend_from_slice(&417u16.to_le_bytes()); // nBlockSize
        fmt.extend_from_slice(&1u16.to_le_bytes()); // nFramesPerBlock
        fmt.extend_from_slice(&0u16.to_le_bytes()); // nCodecDelay

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        body.extend_from_slice(&fmt);

        if let Some(num_frames) = fact_num_frames {
            body.extend_from_slice(b"fact");
            body.extend_from_slice(&4u32.to_le_bytes());
            body.extend_from_slice(&num_frames.to_le_bytes());
        }

        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&data);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Wrap concatenated MPEG audio frames in a minimal RIFF/WAVE container whose `fmt ` chunk
    /// declares `WAVE_FORMAT_MPEGLAYER3` (0x0055).
    fn riff_mpeglayer3(frames: &[Vec<u8>]) -> Vec<u8> {
        riff_mpeglayer3_ex(frames, 16_000, None)
    }

    #[test]
    fn reads_mpeg_layer3_in_wav() {
        let frames = vec![mp3_frame(), mp3_frame()];
        let bytes = riff_mpeglayer3(&frames);
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        // The track is recognised as MP3 at 44.1 kHz.
        let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.codec, CODEC_ID_MP3);
        assert_eq!(params.sample_rate, Some(44_100));

        // Each MPEG frame is emitted as one packet, byte-for-byte, then the stream ends.
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(&packet.data[..], &frames[0][..]);
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(&packet.data[..], &frames[1][..]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn mpeg_in_wave_duration_estimated_from_avg_bytes_per_sec() {
        // 100 identical CBR frames: 128 kbps, 44.1 kHz, stereo, 417 bytes/frame, 1152
        // samples/frame. No `fact` chunk, so duration must be estimated from `nAvgBytesPerSec`.
        let frames: Vec<Vec<u8>> = (0..100).map(|_| mp3_frame()).collect();
        let data_len = (frames.len() * 417) as u64;
        let avg_bytes_per_sec = 16_000u32; // 128 kbps / 8

        let bytes = riff_mpeglayer3_ex(&frames, avg_bytes_per_sec, None);
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        let expected = data_len * 44_100 / u64::from(avg_bytes_per_sec);
        assert_eq!(reader.tracks[0].num_frames, Some(expected));
        assert_eq!(reader.tracks[0].duration, Some(Duration::from(expected)));
    }

    #[test]
    fn mpeg_in_wave_duration_prefers_fact_chunk() {
        let frames: Vec<Vec<u8>> = (0..10).map(|_| mp3_frame()).collect();
        // A `fact` chunk sample count that deliberately differs from the bitrate-based estimate
        // (as it would for a file with encoder delay/padding) must take priority.
        let bytes = riff_mpeglayer3_ex(&frames, 16_000, Some(11_000));
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        assert_eq!(reader.tracks[0].num_frames, Some(11_000));
    }

    #[test]
    fn mpeg_in_wave_seek_lands_within_one_frame() {
        // 200 identical CBR frames spanning ~5.2 s at 44.1 kHz.
        const NUM_FRAMES: usize = 200;
        const SAMPLES_PER_FRAME: u64 = 1152;

        let frames: Vec<Vec<u8>> = (0..NUM_FRAMES).map(|_| mp3_frame()).collect();
        let bytes = riff_mpeglayer3_ex(&frames, 16_000, None);
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        let time_base = reader.tracks[0].time_base.expect("time base");
        let total_samples = NUM_FRAMES as u64 * SAMPLES_PER_FRAME;

        for &target_ts in &[0u64, total_samples / 4, total_samples / 2, total_samples * 3 / 4] {
            let time = time_base.calc_time(Timestamp::new(target_ts as i64)).unwrap();

            let seeked = reader
                .seek(SeekMode::Coarse, SeekTo::Time { time, track_id: None })
                .unwrap_or_else(|e| panic!("seek to ts={target_ts} failed: {e:?}"));

            let delta = seeked.actual_ts.abs_delta(Timestamp::new(target_ts as i64)).get();
            assert!(
                delta <= (mpeg::SEEK_PREROLL_FRAMES as u64 + 1) * SAMPLES_PER_FRAME,
                "seek to ts={target_ts} landed {delta} samples away (actual={})",
                seeked.actual_ts
            );

            // A packet must be readable right after the seek, and its timestamp must not precede
            // the reported seek position.
            let packet = reader.next_packet().unwrap().expect("packet after seek");
            assert!(packet.pts.get() as u64 >= seeked.actual_ts.get() as u64);
        }
    }

    #[test]
    fn mpeg_in_wave_seek_past_end_errors_without_panic() {
        let frames: Vec<Vec<u8>> = (0..10).map(|_| mp3_frame()).collect();
        let bytes = riff_mpeglayer3_ex(&frames, 16_000, None);
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        let far_future = Timestamp::new(1_000_000_000);
        let result =
            reader.seek(SeekMode::Coarse, SeekTo::Timestamp { ts: far_future, track_id: 0 });
        assert!(result.is_err());
    }

    /// Encodes a CBR MP3-in-WAV file with `ffmpeg`, returning its bytes, or `None` if `ffmpeg` is
    /// not available or the encode failed (the caller should skip the test in that case).
    fn make_ffmpeg_mp3_in_wav(
        duration_secs: u32,
        sample_rate: u32,
        bitrate_kbps: u32,
    ) -> Option<Vec<u8>> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "symphonia-riff-mpeg-test-{}-{}-{}-{}.wav",
            std::process::id(),
            duration_secs,
            sample_rate,
            bitrate_kbps
        ));

        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi"])
            .arg("-i")
            .arg(format!("sine=frequency=440:duration={duration_secs}:sample_rate={sample_rate}"))
            .args(["-c:a", "libmp3lame", "-b:a"])
            .arg(format!("{bitrate_kbps}k"))
            .args(["-f", "wav"])
            .arg(&path)
            .status();

        if !matches!(status, Ok(status) if status.success()) {
            return None;
        }

        let data = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        data
    }

    #[test]
    fn mpeg_in_wave_ffmpeg_duration_and_seek() {
        const DURATION_SECS: u32 = 8;
        const SAMPLE_RATE: u32 = 44_100;
        const BITRATE_KBPS: u32 = 128;

        let Some(wav_bytes) = make_ffmpeg_mp3_in_wav(DURATION_SECS, SAMPLE_RATE, BITRATE_KBPS)
        else {
            eprintln!("skipping mpeg_in_wave_ffmpeg_duration_and_seek: ffmpeg not available");
            return;
        };

        let mss = MediaSourceStream::new(Box::new(Cursor::new(wav_bytes)), Default::default());
        let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();

        assert!(reader.is_mpeg);

        let params = reader.tracks[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.codec, CODEC_ID_MP3);
        assert_eq!(params.sample_rate, Some(SAMPLE_RATE));

        let num_frames =
            reader.tracks[0].num_frames.expect("mpeg-in-wave track should report num_frames");

        // The `fact` chunk's sample count includes the LAME encoder delay (typically ~1105-2400
        // samples for MPEG-1 Layer III), so allow generous slack above the raw source sample
        // count when comparing against `duration * sample_rate`.
        let expected_samples = u64::from(DURATION_SECS) * u64::from(SAMPLE_RATE);
        let delta = num_frames.abs_diff(expected_samples);
        assert!(
            delta <= 3 * 1152,
            "num_frames={num_frames} too far from expected={expected_samples} (delta={delta})"
        );

        let time_base = reader.tracks[0].time_base.expect("time base");

        for target_secs in [1u32, 3, 5, 7] {
            let time = Time::try_new(i64::from(target_secs), 0).unwrap();
            let required_ts = time_base.calc_timestamp(time).unwrap();

            let seeked = reader
                .seek(SeekMode::Coarse, SeekTo::Time { time, track_id: None })
                .unwrap_or_else(|e| panic!("seek to {target_secs}s failed: {e:?}"));

            let delta = seeked.actual_ts.abs_delta(required_ts).get();
            // The seek lands on the pre-roll frames (and the frames of the bit reservoir) before
            // the frame containing the target.
            assert!(
                delta <= 12 * 1152,
                "seek to {target_secs}s landed {delta} samples away (target={required_ts}, actual={})",
                seeked.actual_ts
            );

            let packet = reader.next_packet().unwrap().expect("packet after seek");
            assert!(packet.pts.get() as u64 >= seeked.actual_ts.get() as u64);
        }

        // Seeking past the end of the track must fail cleanly, without panicking.
        let past_end = Time::try_new(i64::from(DURATION_SECS) + 10, 0).unwrap();
        let result = reader.seek(SeekMode::Coarse, SeekTo::Time { time: past_end, track_id: None });
        assert!(result.is_err(), "seeking past the end of the track should return an error");
    }

    // -- Helpers -------------------------------------------------------------------------------

    fn open_wav(bytes: Vec<u8>) -> Result<WavReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        WavReader::try_new(mss, FormatOptions::default())
    }

    /// Opens a stream that is not seekable.
    fn open_wav_unseekable(bytes: Vec<u8>) -> Result<WavReader<'static>> {
        let mss = MediaSourceStream::new(
            Box::new(ReadOnlySource::new(Cursor::new(bytes))),
            Default::default(),
        );
        WavReader::try_new(mss, FormatOptions::default())
    }

    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(tag);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            out.push(0);
        }
        out
    }

    fn pcm16_mono_fmt() -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
        fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
        fmt.extend_from_slice(&8000u32.to_le_bytes());
        fmt.extend_from_slice(&16000u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        fmt
    }

    /// Wraps chunks in a RIFF/WAVE form.
    fn riff_wave(riff_len: Option<u32>, chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.iter().flatten().copied().collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_len.unwrap_or(4 + body.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    fn read_all_packets(reader: &mut WavReader<'_>) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_packet().expect("no error before the end of stream") {
            packets.push(packet);
        }
        packets
    }

    /// Gets all the tags, as (key, value), from all the metadata revisions.
    fn all_tags(reader: &mut WavReader<'_>) -> Vec<(String, String)> {
        let mut tags = Vec::new();
        let mut metadata = reader.metadata();

        loop {
            if let Some(revision) = metadata.current() {
                for tag in &revision.media.tags {
                    tags.push((tag.raw.key.clone(), tag.raw.value.to_string()));
                }
            }
            if metadata.pop().is_none() {
                break;
            }
        }
        tags
    }

    /// A minimal ID3v2.4 tag with a TIT2 (title) frame.
    fn id3v2_tag(title: &str) -> Vec<u8> {
        let frame_len = 1 + title.len();
        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3\x04\x00\x00");
        tag.extend_from_slice(&[0, 0, 0, (10 + frame_len) as u8]); // Syncsafe size.
        tag.extend_from_slice(b"TIT2");
        tag.extend_from_slice(&[0, 0, 0, frame_len as u8]); // Syncsafe size.
        tag.extend_from_slice(&[0, 0]); // Flags.
        tag.push(3); // UTF-8.
        tag.extend_from_slice(title.as_bytes());
        tag
    }

    // -- Seeking in block-based (ADPCM) formats ------------------------------------------------

    /// Creates an ADPCM WAVE file of `num_blocks` blocks of `block_align` bytes. The bytes of the
    /// `n`th block are all equal to `n`.
    fn adpcm_wav(format_tag: u16, block_align: u16, num_blocks: usize) -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&format_tag.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes()); // Mono.
        fmt.extend_from_slice(&8000u32.to_le_bytes());
        fmt.extend_from_slice(&4000u32.to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes()); // 4 bits per sample.

        if format_tag == 0x0011 {
            // IMA: cbSize = 2, samples per block.
            fmt.extend_from_slice(&2u16.to_le_bytes());
            fmt.extend_from_slice(&505u16.to_le_bytes());
        }
        else {
            // MS: cbSize = 32, samples per block, 7 coefficient pairs.
            fmt.extend_from_slice(&32u16.to_le_bytes());
            fmt.extend_from_slice(&500u16.to_le_bytes());
            fmt.extend_from_slice(&7u16.to_le_bytes());
            fmt.extend_from_slice(&[0u8; 28]);
        }

        let data: Vec<u8> =
            (0..num_blocks).flat_map(|n| vec![n as u8; usize::from(block_align)]).collect();

        riff_wave(None, &[chunk(b"fmt ", &fmt), chunk(b"data", &data)])
    }

    #[test]
    fn adpcm_seek_lands_on_block_boundary() {
        // (format tag, block align, frames per block)
        for (format_tag, block_align, frames_per_block) in
            [(0x0011u16, 256u16, 505u64), (0x0002, 256, 500)]
        {
            let mut reader = open_wav(adpcm_wav(format_tag, block_align, 12)).unwrap();

            // Packets are made of 2 blocks.
            let frames_per_packet = 2 * frames_per_block;

            let packets = read_all_packets(&mut reader);
            assert_eq!(packets.len(), 6);
            assert_eq!(packets[3].pts.get() as u64, 3 * frames_per_packet);

            for target_packet in [0u64, 1, 3, 5] {
                let required_ts = Timestamp::new((target_packet * frames_per_packet + 17) as i64);

                let seeked = reader
                    .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: required_ts, track_id: 0 })
                    .unwrap();

                assert_eq!(seeked.actual_ts.get() as u64, target_packet * frames_per_packet);

                // The next packet is the one that was seeked to, not the end of the stream.
                let packet = reader.next_packet().unwrap().expect("packet after seek");
                assert_eq!(packet.pts, seeked.actual_ts);
                assert_eq!(packet.data[0] as u64, 2 * target_packet);
                assert_eq!(packet.data.len(), 2 * usize::from(block_align));
            }
        }
    }

    // -- Streams with an unknown length --------------------------------------------------------

    #[test]
    fn stream_of_unknown_length_ends_without_error() {
        // Both the RIFF and data chunk lengths are 0xFFFFFFFF, as written by ffmpeg when writing
        // to a pipe. The stream ends part way through a frame.
        let data: Vec<u8> = (0..3 * 1152 * 2 + 100 * 2 + 1).map(|i| i as u8).collect();

        for seekable in [true, false] {
            let mut payload = Vec::new();
            payload.extend_from_slice(b"data");
            payload.extend_from_slice(&u32::MAX.to_le_bytes());
            payload.extend_from_slice(&data);

            let bytes = riff_wave(Some(u32::MAX), &[chunk(b"fmt ", &pcm16_mono_fmt()), payload]);

            let mut reader =
                if seekable { open_wav(bytes) } else { open_wav_unseekable(bytes) }.unwrap();

            assert_eq!(reader.tracks[0].num_frames, None);

            let packets = read_all_packets(&mut reader);

            let frames: u64 = packets.iter().map(|p| p.dur.get()).sum();
            assert_eq!(frames, 3 * 1152 + 100);

            // Packets contain only whole frames, and no audio is dropped or duplicated.
            let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.iter().copied()).collect();
            assert_eq!(&bytes[..], &data[..bytes.len()]);

            // The end of the stream remains the end of the stream.
            assert!(reader.next_packet().unwrap().is_none());
        }
    }

    #[test]
    fn truncated_data_chunk_ends_without_error() {
        // The data chunk claims 1000 frames, but there are only 300.
        let data = vec![7u8; 600];

        let mut payload = Vec::new();
        payload.extend_from_slice(b"data");
        payload.extend_from_slice(&2000u32.to_le_bytes());
        payload.extend_from_slice(&data);

        let mut reader =
            open_wav(riff_wave(Some(u32::MAX), &[chunk(b"fmt ", &pcm16_mono_fmt()), payload]))
                .unwrap();

        let frames: u64 = read_all_packets(&mut reader).iter().map(|p| p.dur.get()).sum();
        assert_eq!(frames, 300);
    }

    // -- Metadata after the data chunk ---------------------------------------------------------

    fn tagged_wav_after_data(odd_data_len: bool) -> (Vec<u8>, Vec<u8>) {
        let data: Vec<u8> = (0..200 + usize::from(odd_data_len)).map(|i| i as u8).collect();

        let mut info = Vec::new();
        info.extend_from_slice(b"INFO");
        info.extend_from_slice(&chunk(b"INAM", b"Title After\0"));
        info.extend_from_slice(&chunk(b"IART", b"Artist After\0"));

        // The RIFF length is deliberately stale: it only covers the chunks up to the data chunk.
        let fmt = chunk(b"fmt ", &pcm16_mono_fmt());
        let data_chunk = chunk(b"data", &data);
        let riff_len = 4 + fmt.len() + data_chunk.len();

        let file = riff_wave(
            Some(riff_len as u32),
            &[fmt, data_chunk, chunk(b"LIST", &info), chunk(b"id3 ", &id3v2_tag("Id3 Title"))],
        );

        (file, data)
    }

    #[test]
    fn info_and_id3_chunks_after_data_are_read() {
        for odd_data_len in [false, true] {
            let (file, data) = tagged_wav_after_data(odd_data_len);

            let mut reader = open_wav(file).unwrap();

            let tags = all_tags(&mut reader);
            assert!(tags.contains(&("INAM".into(), "Title After".into())), "{tags:?}");
            assert!(tags.contains(&("IART".into(), "Artist After".into())), "{tags:?}");
            assert!(tags.contains(&("TIT2".into(), "Id3 Title".into())), "{tags:?}");

            // The reader is back at the start of the audio data.
            let packets = read_all_packets(&mut reader);
            assert_eq!(packets[0].pts.get(), 0);
            let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.iter().copied()).collect();
            assert_eq!(&bytes[..], &data[..bytes.len()]);
            assert_eq!(bytes.len(), 200);
        }
    }

    #[test]
    fn id3_chunk_before_data_is_read() {
        let data = vec![0u8; 100];
        let file = riff_wave(
            None,
            &[
                chunk(b"fmt ", &pcm16_mono_fmt()),
                chunk(b"ID3 ", &id3v2_tag("Before")),
                chunk(b"data", &data),
            ],
        );

        let mut reader = open_wav(file).unwrap();
        assert!(all_tags(&mut reader).contains(&("TIT2".into(), "Before".into())));
        assert_eq!(reader.tracks[0].num_frames, Some(50));
    }

    #[test]
    fn malformed_chunks_after_data_do_not_fail_the_probe() {
        let data = vec![0u8; 100];

        let mut garbage = Vec::new();
        garbage.extend_from_slice(b"LIST");
        garbage.extend_from_slice(&0x7fff_fff0u32.to_le_bytes());
        garbage.extend_from_slice(b"INFOgarbage");

        for trailer in [
            garbage,
            // A truncated chunk header.
            b"LIS".to_vec(),
            // A malformed ID3 tag.
            chunk(b"id3 ", b"not an id3 tag at all"),
            // Chunk of excessive length.
            [b"id3 ".as_slice(), &u32::MAX.to_le_bytes()].concat(),
        ] {
            let file = riff_wave(
                None,
                &[chunk(b"fmt ", &pcm16_mono_fmt()), chunk(b"data", &data), trailer],
            );

            let mut reader = open_wav(file).unwrap();
            let packets = read_all_packets(&mut reader);
            assert_eq!(packets.iter().map(|p| p.dur.get()).sum::<u64>(), 50);
        }
    }

    #[test]
    fn chunks_after_data_are_ignored_for_unseekable_sources() {
        let (file, _) = tagged_wav_after_data(false);
        let mut reader = open_wav_unseekable(file).unwrap();
        assert!(all_tags(&mut reader).iter().all(|(key, _)| key != "INAM" && key != "TIT2"));
        assert_eq!(read_all_packets(&mut reader).iter().map(|p| p.dur.get()).sum::<u64>(), 100);
    }

    #[test]
    fn oversized_info_strings_are_skipped() {
        let mut info = Vec::new();
        info.extend_from_slice(b"INFO");
        info.extend_from_slice(&chunk(b"ICMT", &vec![b'x'; 2 * 1024 * 1024]));
        info.extend_from_slice(&chunk(b"INAM", b"Kept\0"));

        let file = riff_wave(
            None,
            &[chunk(b"fmt ", &pcm16_mono_fmt()), chunk(b"LIST", &info), chunk(b"data", &[0u8; 4])],
        );

        let mut reader = open_wav(file).unwrap();
        let tags = all_tags(&mut reader);
        assert!(tags.contains(&("INAM".into(), "Kept".into())));
        // The value of the oversized string is skipped.
        assert!(tags.iter().all(|(key, value)| key != "ICMT" || value.is_empty()));
    }

    // -- BW64 and Wave64 -----------------------------------------------------------------------

    #[test]
    fn bw64_is_supported() {
        let pcm_data = vec![0u8; 1000];
        let mut file = create_rf64_test_file(1000, 500, &pcm_data);
        file[0..4].copy_from_slice(b"BW64");

        let reader = open_wav(file).unwrap();
        assert_eq!(reader.tracks[0].num_frames, Some(500));
    }

    fn w64_guid(tag: &[u8; 4]) -> Vec<u8> {
        let mut guid = tag.to_vec();
        guid.extend_from_slice(&[
            0xf3, 0xac, 0xd3, 0x11, 0x8c, 0xd1, 0x00, 0xc0, 0x4f, 0x8e, 0xdb, 0x8a,
        ]);
        guid
    }

    fn w64_chunk(guid: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = guid.to_vec();
        out.extend_from_slice(&(24 + payload.len() as u64).to_le_bytes());
        out.extend_from_slice(payload);
        // Chunks are padded to a multiple of 8 bytes.
        out.resize(out.len().next_multiple_of(8), 0);
        out
    }

    fn make_w64(data: &[u8]) -> Vec<u8> {
        let mut riff_guid = b"riff".to_vec();
        riff_guid.extend_from_slice(&[
            0x2e, 0x91, 0xcf, 0x11, 0xa5, 0xd6, 0x28, 0xdb, 0x04, 0xc1, 0x00, 0x00,
        ]);

        // An unknown chunk (the LIST chunk GUID) to be skipped, before the data.
        let mut list_guid = b"list".to_vec();
        list_guid.extend_from_slice(&[
            0x2f, 0x91, 0xcf, 0x11, 0xa5, 0xd6, 0x28, 0xdb, 0x04, 0xc1, 0x00, 0x00,
        ]);

        let chunks = [
            w64_chunk(&w64_guid(b"fmt "), &pcm16_mono_fmt()),
            w64_chunk(&list_guid, b"odd length payload"),
            w64_chunk(&w64_guid(b"data"), data),
        ]
        .concat();

        let mut out = riff_guid;
        out.extend_from_slice(&(16 + 8 + 16 + chunks.len() as u64).to_le_bytes());
        out.extend_from_slice(&w64_guid(b"wave"));
        out.extend_from_slice(&chunks);
        out
    }

    #[test]
    fn wave64_is_supported() {
        // An odd number of frames' bytes, so the data chunk is padded.
        let data: Vec<u8> = (0..202).map(|i| i as u8).collect();

        let mut reader = open_wav(make_w64(&data)).unwrap();

        let params = reader.tracks[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.sample_rate, Some(8000));
        assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_PCM_S16LE);
        assert_eq!(reader.tracks[0].num_frames, Some(101));

        let packets = read_all_packets(&mut reader);
        let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.iter().copied()).collect();
        assert_eq!(bytes, data);

        // Seeking.
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1152), track_id: 0 });
        assert!(seeked.is_err());
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0 })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), 0);
        assert_eq!(reader.next_packet().unwrap().unwrap().data.len(), 202);
    }

    #[test]
    fn wave64_probe_scoring() {
        let mss =
            MediaSourceStream::new(Box::new(Cursor::new(make_w64(&[0u8; 8]))), Default::default());
        let mut mss = mss;
        let score = WavReader::score(ScopedStream::new(&mut mss, 64)).unwrap();
        assert!(matches!(score, Score::Supported(255)));

        let mut mss = MediaSourceStream::new(
            Box::new(Cursor::new(b"riff0123456789abcdefghijklmnop".to_vec())),
            Default::default(),
        );
        let score = WavReader::score(ScopedStream::new(&mut mss, 64)).unwrap();
        assert!(matches!(score, Score::Unsupported));
    }

    // -- MPEG audio in WAVE --------------------------------------------------------------------

    #[test]
    fn mpeg_layer2_in_wav_is_supported() {
        // MPEG-1 Layer II, 128 kbps, 44.1 kHz, stereo: 417 bytes.
        let mut frame = vec![0u8; 417];
        frame[0..4].copy_from_slice(&0xFFFD_8000u32.to_be_bytes());
        let frames = vec![frame.clone(), frame.clone(), frame];

        let mut bytes = riff_mpeglayer3(&frames);
        // Change the format tag from WAVE_FORMAT_MPEGLAYER3 (0x0055) to WAVE_FORMAT_MPEG (0x0050).
        assert_eq!(&bytes[20..22], &0x0055u16.to_le_bytes());
        bytes[20..22].copy_from_slice(&0x0050u16.to_le_bytes());

        let mut reader = open_wav(bytes).unwrap();

        let params = reader.tracks[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_MP2);
        assert_eq!(params.sample_rate, Some(44_100));

        assert_eq!(read_all_packets(&mut reader).len(), 3);
    }

    /// MPEG-1 Layer III frames of varying bit-rate and padding (VBR), 44.1 kHz stereo. The two
    /// bytes after the header of the `n`th frame identify `n`.
    fn vbr_mp3_frames(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|n| {
                // 128 kbps (417 bytes, 418 with padding), or 320 kbps (1044 bytes).
                let (header, len) = match n % 5 {
                    0 | 3 => (0xFFFB_E000u32, 1044),
                    1 => (0xFFFB_9200u32, 418),
                    _ => (0xFFFB_9000u32, 417),
                };
                let mut frame = vec![0u8; len];
                frame[0..4].copy_from_slice(&header.to_be_bytes());
                // Identify the frame in its last bytes, leaving the side information (and thus
                // `main_data_begin`) zero.
                frame[len - 2] = (n / 100) as u8;
                frame[len - 1] = (n % 100) as u8;
                frame
            })
            .collect()
    }

    #[test]
    fn mpeg_in_wave_seek_is_exact_for_variable_frame_lengths() {
        const NUM_FRAMES: usize = 600;
        const SAMPLES_PER_FRAME: u64 = 1152;

        let frames = vbr_mp3_frames(NUM_FRAMES);
        let mut reader = open_wav(riff_mpeglayer3_ex(&frames, 16_000, None)).unwrap();

        for &index in &[0usize, 1, 2, 77, 299, 300, 450, 598, 599] {
            // The target is in the middle of the frame.
            let required_ts = Timestamp::new((index as u64 * SAMPLES_PER_FRAME + 500) as i64);

            let seeked = reader
                .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: required_ts, track_id: 0 })
                .unwrap_or_else(|e| panic!("seek to frame {index} failed: {e:?}"));

            // Decoding starts a fixed number of frames before the target frame.
            let start = index.saturating_sub(mpeg::SEEK_PREROLL_FRAMES);
            assert_eq!(seeked.actual_ts.get() as u64, start as u64 * SAMPLES_PER_FRAME);

            // The pre-roll frames follow, and then the target frame.
            for i in start..=index {
                let packet = reader.next_packet().unwrap().expect("packet after seek");
                assert_eq!(packet.pts.get() as u64, i as u64 * SAMPLES_PER_FRAME);
                assert_eq!(&packet.data[..], &frames[i][..]);
            }

            if index + 1 < NUM_FRAMES {
                let packet = reader.next_packet().unwrap().expect("packet after target");
                assert_eq!(packet.pts.get() as u64, (index as u64 + 1) * SAMPLES_PER_FRAME);
                assert_eq!(&packet.data[..], &frames[index + 1][..]);
            }
        }

        // Past the last frame.
        let result = reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new((NUM_FRAMES as u64 * SAMPLES_PER_FRAME) as i64),
                track_id: 0,
            },
        );
        assert!(result.is_err());
    }
}
