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

use log::{debug, error};

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
    /// Sampling rate of the MPEG elementary stream (0 if `is_mpeg` is false or unknown).
    mpeg_sample_rate: u32,
    /// Average bytes/second of the MPEG elementary stream, used for coarse seeking and duration
    /// estimation when neither a `fact` chunk nor ds64 sample count is available (0 if `is_mpeg`
    /// is false or unknown).
    mpeg_bytes_per_sec: u64,
    /// Number of PCM samples per MPEG frame (0 if `is_mpeg` is false or unknown). Used to convert
    /// a seek target directly into a frame index, so coarse seeking lands on (or very near) an
    /// actual frame boundary instead of an arbitrary byte offset that may fall in the middle of a
    /// frame's body.
    mpeg_samples_per_frame: u64,
}

impl<'s> WavReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, mut opts: FormatOptions) -> Result<Self> {
        // A Wave file is one large RIFF chunk, with the actual meta and audio data contained in
        // nested chunks. Therefore, the file starts with a RIFF chunk header (chunk ID & size).

        // The top-level chunk has the RIFF or RF64 chunk ID. This is also the file marker.
        let marker = mss.read_quad_bytes()?;

        let is_rf64 = match marker {
            WAVE_STREAM_MARKER => false,
            RF64_STREAM_MARKER => true,
            _ => return unsupported_error("wav: missing riff/rf64 stream marker"),
        };

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

        // When ffmpeg encodes wave to stdout the riff (parent) and data (child) chunk lengths are
        // (2^32)-1 since the size is not known ahead of time. For RF64 files, the riff length is
        // also 0xFFFFFFFF.
        let riff_data_len = if riff_len < u32::MAX { Some(riff_len - 4) } else { None };

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
            let chunk = riff_chunks.next(&mut mss)?;

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

                    // For MPEG audio the elementary stream is authoritative for the exact codec
                    // (Layer I/II/III) and sample rate, so refine the codec parameters from the
                    // first frame, then rewind so the first packet is read in full. Also derive
                    // the average bytes/second used for duration estimation and coarse seeking:
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
                            } else {
                                u64::from(first.bitrate) / 8
                            };
                        }
                        mss.seek_buffered(data_start_pos);
                    }

                    // Create the track.
                    let mut track = Track::new(0);

                    track.with_codec_params(CodecParameters::Audio(codec_params));

                    let Some(packet_info) = packet_info else {
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
                            } else if let Some(fact) = &fact {
                                Some(u64::from(fact.num_frames))
                            } else if mpeg_bytes_per_sec > 0 && mpeg_sample_rate > 0 {
                                Some(
                                    data_len.saturating_mul(u64::from(mpeg_sample_rate))
                                        / mpeg_bytes_per_sec,
                                )
                            } else {
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
                        } else {
                            append_data_params(&mut track, data_len, &packet_info);

                            // For RF64 files, prefer the sample count from ds64 over the computed
                            // value. For standard WAV, prefer fact chunk over computed value.
                            // Applied after append_data_params so the authoritative value wins.
                            if let Some(sample_count) = rf64_sizes.sample_count {
                                track.with_num_frames(sample_count);
                            } else if let Some(fact) = &fact {
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
                        mpeg_sample_rate,
                        mpeg_bytes_per_sec,
                        mpeg_samples_per_frame,
                    });
                }
            }
        }
    }

    /// Seeks an MPEG-in-WAVE track to the MPEG frame closest to `required_ts`. A coarse byte
    /// offset is estimated from the average byte rate, then the reader resynchronises to the
    /// next valid MPEG frame header from there and reports that frame's (aligned) timestamp.
    fn seek_mpeg(&mut self, required_ts: Timestamp) -> Result<SeekedTo> {
        if self.mpeg_bytes_per_sec == 0 || self.mpeg_sample_rate == 0 || self.mpeg_samples_per_frame == 0
        {
            return seek_error(SeekErrorKind::Unseekable);
        }

        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let data_end_pos = self.data_end_pos.unwrap_or(u64::MAX);

        debug!("seeking mpeg-in-wave to ts={required_ts}");

        // Assume a constant frame size (in both samples and bytes) and convert the desired
        // timestamp directly into a frame index. Landing on a presumed frame boundary (rather
        // than an arbitrary interpolated byte offset) avoids scanning through the middle of a
        // frame's body, where a run of high-entropy bytes can occasionally look like a false
        // sync.
        let per_frame = self.mpeg_samples_per_frame;
        let frame_len =
            (self.mpeg_bytes_per_sec * per_frame / u64::from(self.mpeg_sample_rate)).max(1);
        let frame_index = (required_ts.get() as u64) / per_frame;

        let seek_pos = self
            .data_start_pos
            .saturating_add(frame_index.saturating_mul(frame_len))
            .min(data_end_pos);

        self.reader.seek(SeekFrom::Start(seek_pos))?;

        // Resynchronise to the next valid MPEG frame header from the estimated position. Use the
        // stricter, verified resync (not the plain frame reader used by `next_packet`) since we
        // may be landing in the middle of a frame body, where a false sync is possible.
        let Some((format, frame)) = mpeg::seek_sync_frame(&mut self.reader, data_end_pos)? else {
            return seek_error(SeekErrorKind::OutOfRange);
        };

        // The byte offset of the frame's header (the frame has just been fully consumed).
        let frame_start_pos = self.reader.pos() - frame.len() as u64;

        // Rewind so the resynced frame is read again, in full, as the first post-seek packet.
        self.reader.seek_buffered_rev(frame.len());

        // Recover the landed frame's index from its byte offset using the *same* assumed frame
        // length used above (rather than re-deriving it via sample-rate/byte-rate arithmetic),
        // so that a frame found exactly where expected reports back exactly the timestamp that
        // was targeted, without compounding additional rounding error.
        let landed_offset = frame_start_pos.saturating_sub(self.data_start_pos);
        let landed_frame_index = landed_offset / frame_len;
        let aligned_ts = landed_frame_index.saturating_mul(format.samples_per_frame.max(1));

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
        // Perform simple scoring by testing that the RIFF/RF64 stream marker and RIFF form are
        // both valid for WAVE.
        let marker = src.read_quad_bytes()?;
        src.ignore_bytes(4)?;
        let riff_form = src.read_quad_bytes()?;

        let is_valid_marker = marker == WAVE_STREAM_MARKER || marker == RF64_STREAM_MARKER;

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

        // Calculate the absolute byte offset of the desired audio frame.
        let seek_pos =
            self.data_start_pos + (actual_ts.get() as u64 * self.packet_info.block_size.get());

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
                delta <= SAMPLES_PER_FRAME,
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
    fn make_ffmpeg_mp3_in_wav(duration_secs: u32, sample_rate: u32, bitrate_kbps: u32) -> Option<Vec<u8>> {
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
            assert!(
                delta <= 1152,
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
}
