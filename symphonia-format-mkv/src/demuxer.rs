// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::num::NonZero;
use std::sync::Arc;

use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MKV;
use symphonia_core::io::*;
use symphonia_core::meta::{
    Metadata, MetadataBuilder, MetadataLog, StandardTag, StandardVisualKey, Tag, Visual,
};
use symphonia_core::support_format;
use symphonia_core::units::TimeBase;

use log::{info, warn};
use symphonia_common::xiph::audio::opus;
use symphonia_metadata::utils::images::try_get_image_info;

use crate::codecs::make_track_codec_params;
use crate::ebml::{
    EbmlElementInfo, EbmlError, EbmlIterator, EbmlSchema, ReadEbml, read_unsigned_vint,
};
use crate::lacing::{Frame, extract_frames};
use crate::schema::{MkvElement, MkvSchema};
use crate::segment::{
    AttachmentsElement, BlockGroupElement, ChaptersElement, CuesElement, EbmlHeaderElement,
    InfoElement, MKV_METADATA_INFO, MatroskaTicks, NonZeroMatroskaTicks, SeekHeadElement,
    SegmentTicks, SignedTrackTicks, TagsElement, TargetTagsMap, TrackTicks, TracksElement,
    nanos_to_ticks, ticks_to_nanos,
};
use crate::timeline::{Cursor, PacketDurations};

const MKV_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MKV, short_name: "matroska", long_name: "Matroska / WebM" };

/// Get the constant number of frames per block of a lossless codec (all but the last block of a
/// stream have this many frames) from the codec's private data.
///
/// This allows the exact timestamps of blocks to be recovered when the muxer did not write a
/// default duration, since the timestamps in a Matroska file are only as precise as the
/// timestamp scale (usually 1ms).
fn fixed_block_samples(codec_id: &str, codec_private: Option<&[u8]>) -> Option<u64> {
    let data = codec_private?;

    let samples = match codec_id {
        "A_FLAC" => {
            // "fLaC", followed by the STREAMINFO block: a 4 byte block header, then the minimum
            // and maximum block sizes (16-bit, big-endian). Only a fixed block size is constant.
            let streaminfo = data.strip_prefix(b"fLaC")?.get(4..8)?;
            let min = u16::from_be_bytes([streaminfo[0], streaminfo[1]]);
            let max = u16::from_be_bytes([streaminfo[2], streaminfo[3]]);
            if min != max {
                return None;
            }
            u64::from(min)
        }
        "A_ALAC" => {
            // The magic cookie, optionally wrapped in an "alac" atom (size, "alac", version and
            // flags). The first field of the ALAC configuration is the frame length (32-bit,
            // big-endian).
            let config = match data.get(4..8) {
                Some(b"alac") => data.get(12..)?,
                _ => data,
            };
            u64::from(u32::from_be_bytes(config.get(0..4)?.try_into().ok()?))
        }
        _ => return None,
    };

    // Sanity check.
    (samples > 0 && samples <= 1 << 20).then_some(samples)
}

/// Get the default seek pre-roll of a codec.
///
/// Decoders of codecs with overlapping transforms and/or inter-frame dependencies (the bit
/// reservoir of MPEG audio, the lapped transforms of AAC and Vorbis) need to decode a number of
/// frames preceding the seek target before their output is correct. Only Opus is required to
/// signal this to the demuxer (`SeekPreRoll`), so muxers do not write it for other codecs.
fn default_seek_pre_roll(codec_id: &str) -> MatroskaTicks {
    const MILLIS: u64 = 1_000_000;

    match codec_id {
        id if id.starts_with("A_MPEG/L") || id.starts_with("A_AAC") || id == "A_VORBIS" => {
            MatroskaTicks::from(200 * MILLIS)
        }
        _ => MatroskaTicks::from(0),
    }
}

/// Create a media-level `Visual` for an image attachment (e.g., cover art), if the attachment
/// is an image.
fn make_attachment_visual(attachment: &Attachment) -> Option<Visual> {
    let Attachment::File(file) = attachment
    else {
        return None;
    };

    let image_info = try_get_image_info(&file.data);

    // The attachment must either be recognized as an image, or declared as one.
    let declared_image = file.media_type.as_deref().is_some_and(|media_type| {
        media_type.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    });

    if image_info.is_none() && !declared_image {
        return None;
    }

    // The Matroska specification names cover art "cover", "small_cover", "cover_land", or
    // "small_cover_land" (with any image extension).
    let name = file.name.to_ascii_lowercase();
    let stem = name.rsplit_once('.').map_or(name.as_str(), |(stem, _)| stem);

    let usage = if stem.contains("cover") || stem.contains("front") {
        Some(StandardVisualKey::FrontCover)
    }
    else if stem.contains("back") {
        Some(StandardVisualKey::BackCover)
    }
    else {
        None
    };

    Some(Visual {
        media_type: image_info
            .as_ref()
            .map(|info| info.media_type.clone())
            .or_else(|| file.media_type.clone()),
        dimensions: image_info.as_ref().map(|info| info.dimensions),
        color_mode: image_info.as_ref().map(|info| info.color_mode),
        usage,
        tags: Default::default(),
        data: file.data.clone(),
    })
}

pub struct TrackState {
    /// The Matroska track number (Symphonia's track ID).
    track_num: u32,
    /// The default frame duration.
    pub(crate) default_frame_duration: Option<NonZeroMatroskaTicks>,
    /// The codec delay.
    pub(crate) codec_delay: MatroskaTicks,
    /// The track's timebase. For audio tracks this is always the reciprocal of the sample rate.
    pub(crate) track_time_base: TimeBase,
    /// The segment's timestamp scale (the number of nanoseconds in a Segment tick).
    pub(crate) timestamp_scale: u64,
    /// The track's timestamp scale.
    pub(crate) track_timestamp_scale: f64,
    /// The amount of lead-in (in Matroska ticks) a decoder needs to fully re-converge its
    /// internal state after a random seek before the decoded output is usable (Matroska
    /// `SeekPreRoll`). Mandatory for Opus per RFC 7845 section 4.6 (>= 80ms/3840 samples).
    /// See [`Self::effective_seek_pre_roll`].
    pub(crate) seek_pre_roll: MatroskaTicks,
    /// `true` if the track is Opus.
    is_opus: bool,
    /// `true` if an Opus track has been seen to contain SILK or Hybrid packets.
    silk_seen: bool,
    /// The track's sample rate, if known (audio tracks only). Used to convert `codec_delay`
    /// and `DiscardPadding` into an exact sample count for gapless trimming.
    pub(crate) sample_rate: Option<u32>,
    /// The codec delay in samples (exact).
    codec_delay_samples: u64,
    /// The number of bytes in a frame of audio, if the track is PCM audio.
    pcm_frame_bytes: Option<NonZero<u32>>,
    /// The constant number of frames in all but the last block of the track, if the codec
    /// signals it (e.g., FLAC with a fixed block size, ALAC).
    fixed_block_samples: Option<u64>,
    /// The maximum difference, in Track ticks, between the timestamp of a block and its expected
    /// timestamp for the timestamps to be considered equal. This is the precision of the
    /// timestamp of a block.
    pts_tolerance: u64,
    /// The grid of expected timestamps for blocks, if the track has a constant frame duration.
    grid: Option<PtsGrid>,
    /// The parser of packet durations, if the codec carries the duration of a packet in the
    /// packet (Vorbis, Opus). Used to compute an exact timeline.
    exact: Option<PacketDurations>,
}

impl TrackState {
    /// Get the pre-roll to use when seeking the track.
    ///
    /// For Opus the 80 ms signalled by the container (RFC 7845 section 4.6) only brings a reset
    /// decoder close to a continuous decode. Use the longer pre-roll that makes the output
    /// identical (CELT) or as close as it gets (SILK), see
    /// `symphonia_common::xiph::audio::opus::SEEK_PREROLL_CELT`/`SEEK_PREROLL_SILK`.
    fn effective_seek_pre_roll(&self) -> MatroskaTicks {
        if !self.is_opus {
            return self.seek_pre_roll;
        }

        let frames = opus::seek_preroll(self.silk_seen);
        let nanos = u128::from(frames) * 1_000_000_000 / 48_000;

        self.seek_pre_roll.max(MatroskaTicks::from(u64::try_from(nanos).unwrap_or(u64::MAX)))
    }
}

/// A grid of the timestamps at which the blocks of a track are expected to start.
#[derive(Copy, Clone, Debug)]
struct PtsGrid {
    /// The timestamp of the first block of the track in Track ticks.
    anchor: i64,
    /// The duration of a block in Track ticks. Never 0.
    step: u64,
}

impl TrackState {
    /// Get the number of leading decoded samples of a frame with timestamp `pts` and duration
    /// `dur` that must be discarded to account for the codec delay.
    fn codec_delay_trim(&self, pts: SignedTrackTicks, dur: TrackTicks) -> u64 {
        if self.sample_rate.is_none() || self.codec_delay_samples == 0 || pts.get() >= 0 {
            return 0;
        }

        // The delay of a Vorbis stream is the frames the decoder discards from the first packet.
        // They are already trimmed from it, and are not decoded from the packets after it.
        if self.exact.as_ref().is_some_and(PacketDurations::discards_delay) {
            return 0;
        }

        // The timestamp of a frame is its timestamp in the block minus the codec delay. Therefore,
        // for the frames in the first `codec_delay` of the stream, which are the frames that have
        // negative timestamps, the number of samples to discard is exactly the number of samples
        // until the true start of the stream (timestamp 0). Note the timebase of an audio track is
        // the reciprocal of the sample rate so ticks and samples are interchangeable.
        let remaining = pts.get().unsigned_abs().min(self.codec_delay_samples);

        // The frame cannot be trimmed by more than its duration, if the duration is known.
        match dur.get() {
            0 => remaining,
            dur_samples => remaining.min(dur_samples),
        }
    }

    /// Get the duration of a block of PCM audio of `len` bytes.
    pub(crate) fn pcm_duration(&self, len: usize) -> Option<TrackTicks> {
        self.pcm_frame_bytes.map(|bytes| TrackTicks::from((len as u64) / u64::from(bytes.get())))
    }

    /// Snap the timestamp of a block to the track's timestamp grid, if the track has one, and the
    /// timestamp is close to a grid line.
    ///
    /// The timestamps of blocks are only as precise as the segment's timestamp scale (usually
    /// 1ms). However, the blocks of audio tracks with a constant frame duration (e.g., MP3, AAC,
    /// Opus, FLAC, PCM) start at exact multiples of the frame duration (in samples) from the
    /// start of the track. This recovers the exact, sample accurate, timestamp of a block.
    pub(crate) fn snap_pts(&self, pts: SignedTrackTicks) -> SignedTrackTicks {
        match &self.grid {
            Some(grid) => {
                let step = i128::from(grid.step);
                let rel = i128::from(pts.get()) - i128::from(grid.anchor);
                let snapped = (rel + step / 2).div_euclid(step) * step;

                if snapped.abs_diff(rel) <= u128::from(self.pts_tolerance) {
                    i64::try_from(i128::from(grid.anchor) + snapped)
                        .map(SignedTrackTicks::from)
                        .unwrap_or(pts)
                }
                else {
                    pts
                }
            }
            None => pts,
        }
    }

    /// Snap the duration of a frame to the duration of the frames of the track's timestamp grid,
    /// if the track has one, and the duration is close to it. Durations written by muxers are
    /// rounded like timestamps. The durations of PCM blocks are exact, and are not snapped.
    pub(crate) fn snap_dur(&self, dur: TrackTicks) -> TrackTicks {
        match &self.grid {
            Some(grid)
                if self.pcm_frame_bytes.is_none()
                    && dur.get().abs_diff(grid.step) <= self.pts_tolerance =>
            {
                TrackTicks::from(grid.step)
            }
            _ => dur,
        }
    }

    /// If the exact position of a block can only be known by scanning the stream (see
    /// `MkvReader::ensure_timeline_index`).
    fn needs_timeline_index(&self) -> bool {
        self.exact.is_some()
    }
}

/// Matroska (MKV) and WebM demultiplexer.
///
/// `MkvReader` implements a demuxer for the Matroska and WebM formats.
pub struct MkvReader<'s> {
    /// Iterator over EBML element headers
    iter: EbmlIterator<MediaSourceStream<'s>, MkvSchema>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    track_states: HashMap<u32, TrackState>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    cues: Option<CuesElement>,
    current_cluster: Option<ClusterState>,
    frames: VecDeque<Frame>,
    /// For each track, the timestamp at which the previous packet read from the track ended.
    last_pts_end: HashMap<u32, i64>,
    /// For each track with an exact timeline, the position and state of the next packet that is
    /// read from the track. Not present after a jump in the stream.
    cursors: HashMap<u32, Cursor>,
    /// For each track with an exact timeline, the cursor at the start of every block of the
    /// track (sorted by the position of the block in the stream). Built by the first seek.
    timeline_index: HashMap<u32, Vec<(u64, Cursor)>>,
    /// If the stream has been scanned to build the timeline index.
    timeline_indexed: bool,
    /// If the stream is being scanned to build the timeline index.
    indexing: bool,
    /// The position of the first Cluster relative to the start of the Segment, if known.
    first_cluster_pos: Option<u64>,
    /// If the media source is seekable.
    is_seekable: bool,
}

#[derive(Copy, Clone, Debug)]
struct ClusterState {
    /// The cluster timestamp in Segment ticks..
    timestamp: Option<SegmentTicks>,
    /// The start position in bytes of the cluster.
    start: u64,
}

impl<'s> MkvReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Get the total length of the stream, if possible.
        let (is_seekable, total_len) = (mss.is_seekable(), mss.byte_len());

        match total_len {
            Some(len) if is_seekable => info!("stream is seekable with len={len} bytes."),
            _ => (),
        }

        let mut it = EbmlIterator::new(mss, MkvSchema, total_len);

        // Read the EBML header.
        let ebml = it.next_element::<EbmlHeaderElement>()?;

        if !matches!(ebml.doc_type.as_str(), "matroska" | "webm") {
            return unsupported_error("mkv: not a matroska / webm file");
        }

        // Read the root element, Segment.
        let segment_pos = match it.next_header()? {
            Some(elem) if elem.element_type() == MkvElement::Segment => elem.data_pos(),
            _ => return unsupported_error("mkv: missing segment element"),
        };

        // Descend into the Segment element.
        it.push_element()?;

        let mut segment_tracks = None;
        let mut info = None;
        let mut cues = None;
        let mut current_cluster = None;
        let mut seek_positions = Vec::new();
        let mut tags = Vec::new();
        let mut attachments = None;
        let mut chapters = None;

        while let Ok(Some(header)) = it.next_header() {
            match header.element_type() {
                MkvElement::SeekHead => {
                    let seek_head = it.read_master_element::<SeekHeadElement>()?;
                    for element in seek_head.seeks.into_vec() {
                        let element_type = match it.schema().get_element_info(element.id as u32) {
                            Some(info) => info.element_type(),
                            None => continue,
                        };
                        seek_positions.push((element_type, element.position));
                    }
                }
                MkvElement::Tracks => {
                    segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                }
                MkvElement::Info => {
                    info = Some(it.read_master_element::<InfoElement>()?);
                }
                MkvElement::Cues => {
                    cues = Some(it.read_master_element::<CuesElement>()?);
                }
                MkvElement::Tags => {
                    // Multiple tags element per segment allowed.
                    tags.push(it.read_master_element::<TagsElement>()?);
                }
                MkvElement::Cluster => {
                    // Set state for current cluster for the first call of `next_element`.
                    current_cluster =
                        Some(ClusterState { timestamp: None, start: header.pos() - segment_pos });

                    // Don't look forward into the stream since we can't be sure that we'll
                    // find anything useful.
                    break;
                }
                MkvElement::Attachments => {
                    // Only one attachments element per segment is expected.
                    if attachments.is_some() {
                        log::warn!("unexpected attachments element");
                    }
                    attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                }
                MkvElement::Chapters => {
                    // Only one chapters element per segment is expected.
                    if chapters.is_some() {
                        log::warn!("unexpected chapters element");
                    }
                    chapters = Some(it.read_master_element::<ChaptersElement>()?);
                }
                other => {
                    log::debug!("top-level scan ignored element {other:?}");
                }
            }
        }

        if is_seekable {
            // All elements preceeding the element iterator's current position have already been
            // read and do not need to be revisited.
            seek_positions.retain(|sp| sp.1 >= it.pos());
            // Make sure we don't jump backwards unnecessarily.
            seek_positions.sort_by_key(|sp| sp.1);

            for (_, pos) in seek_positions {
                // Ascend back to the segment element.
                it.pop_elements_upto(MkvElement::Segment)?;

                // Seek the iterator to the child element.
                it.seek_to_child(pos)?;

                // Resume iteration.
                let element_type = match it.next_header()? {
                    Some(header) => header.element_type(),
                    _ => continue,
                };

                // Safety: The element type or position may be incorrect. The element iterator will
                // validate the type (as declared in the header) of the element at the seeked
                // position against the element type asked to be read.
                match element_type {
                    MkvElement::Tracks => {
                        segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                    }
                    MkvElement::Info => {
                        info = Some(it.read_master_element::<InfoElement>()?);
                    }
                    MkvElement::Tags => {
                        // Multiple tags element per segment allowed.
                        tags.push(it.read_master_element::<TagsElement>()?);
                    }
                    MkvElement::Cues => {
                        cues = Some(it.read_master_element::<CuesElement>()?);
                    }
                    MkvElement::Attachments => {
                        // Only one attachments element per segment is expected.
                        if attachments.is_some() {
                            log::warn!("unexpected attachments element after meta seek");
                        }
                        attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                    }
                    MkvElement::Chapters => {
                        // Only one chapters element per segment is expected.
                        if chapters.is_some() {
                            log::warn!("unexpected chapters element after meta seek");
                        }
                        chapters = Some(it.read_master_element::<ChaptersElement>()?)
                    }
                    _ => (),
                }
            }
        }

        let segment_tracks =
            segment_tracks.ok_or(Error::DecodeError("mkv: missing Tracks element"))?;

        // If seekable, seek to the start of the first cluster, if known, or the start of the
        // segment. If unseekable, the element iterator is already positioned at the start of the
        // first cluster.
        if is_seekable {
            let cluster_pos = current_cluster.as_ref().map(|cluster| cluster.start).unwrap_or(0);
            it.seek_to_child(cluster_pos)?;
            let _ = it.next_header()?;
        }

        // Descend into the cluster.
        it.push_element()?;

        let info = info.ok_or(Error::DecodeError("mkv: missing Info element"))?;

        // Create a hashmap of all per-target tags (edition, chapter, & attachment tags).
        let mut per_target_tags: TargetTagsMap = Default::default();

        segment_tracks.get_target_uids(&mut per_target_tags);

        if let Some(chapters) = &chapters {
            chapters.get_target_uids(&mut per_target_tags);
        }
        if let Some(attachments) = &attachments {
            attachments.get_target_uids(&mut per_target_tags);
        }

        // Begin with externally provided metadata and chapters.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Post-process all tag elements into metadata revisions, while also collecting per-target
        // tags.
        let is_video = segment_tracks.tracks.as_ref().iter().any(|t| t.video.is_some());

        let mut revisions = tags
            .into_iter()
            .map(|tag| tag.into_metadata(&mut per_target_tags, is_video))
            .collect::<Vec<_>>();

        // ffmpeg (and other encoders) commonly write a file's title into the Segment's `Info.Title`
        // element rather than as a `TITLE` `SimpleTag`, especially for simple, single-track,
        // album-less files. Surface it as the track title, merged into the first tags-derived
        // metadata revision so it lands alongside the rest of the tags in the same (current)
        // revision, unless a `Tags`-sourced title standard tag is already present.
        if let Some(title) = info.title.as_deref() {
            let has_track_title = revisions.first().is_some_and(|rev| {
                rev.media.tags.iter().any(|t| matches!(t.std, Some(StandardTag::TrackTitle(_))))
            });

            if !has_track_title {
                let tag = Tag::new_from_parts(
                    "TITLE",
                    title,
                    Some(StandardTag::TrackTitle(Arc::new(title.to_string()))),
                );

                match revisions.first_mut() {
                    Some(rev) => rev.media.tags.push(tag),
                    None => {
                        let mut builder = MetadataBuilder::new(MKV_METADATA_INFO);
                        builder.add_tag(tag);
                        revisions.push(builder.build());
                    }
                }
            }
        }

        // Process attachments element.
        let attachments = attachments
            .map(|attachments| attachments.into_attachments(&mut per_target_tags))
            .unwrap_or_default();

        // Image attachments (i.e., cover art) are also exposed as media-level visuals in the
        // (first) metadata revision.
        let visuals: Vec<Visual> = attachments.iter().filter_map(make_attachment_visual).collect();

        if !visuals.is_empty() {
            if revisions.is_empty() {
                revisions.push(MetadataBuilder::new(MKV_METADATA_INFO).build());
            }
            revisions[0].media.visuals.extend(visuals);
        }

        for rev in revisions {
            metadata.push(rev);
        }

        // Post-process chapters element.
        let chapters = chapters
            .map(|chapters| chapters.into_chapter_group(&mut per_target_tags))
            .unwrap_or(opts.external_data.chapters);

        // Should TimeBase use a u64/u64 rational?
        // Reduce the timebase to reduce the chance of overflows later.
        let time_base = TimeBase::new(
            info.timestamp_scale
                .try_into()
                .map_err(|_| Error::Unsupported("mkv: timestamp scale too large (report this)"))?,
            NonZero::new(1_000_000_000).expect("1_000_000_000 is non-zero"),
        )
        .reduce();

        let mut tracks = Vec::new();
        let mut track_states = HashMap::new();

        for track in segment_tracks.tracks {
            // Extract the sample rate (if this is an audio track) before `track` is consumed by
            // `make_track_codec_params` below. It is used to convert the mandatory `codec_delay`
            // and `DiscardPadding` gapless-trims into exact sample counts.
            let sample_rate = track
                .audio
                .as_ref()
                .map(|audio| {
                    // Opus is always decoded at 48kHz, regardless of the sampling frequency of
                    // the track which describes the rate of the original source.
                    if track.codec_id == "A_OPUS" {
                        48_000
                    }
                    else {
                        audio.sampling_frequency.round() as u32
                    }
                })
                .and_then(NonZero::new);

            // The timebase of an audio track is the reciprocal of its sample rate (like all other
            // containers), such that the timestamps, durations, and trims of packets are
            // expressed in frames (as required of `Packet`), and are not limited to the
            // granularity of the segment's timestamp scale (usually 1ms). Otherwise, the track's
            // timebase is the timebase of the segment, scaled by the track timestamp scale.
            let track_time_base = match sample_rate {
                Some(rate) => TimeBase::new(NonZero::new(1).expect("1 is non-zero"), rate),
                None => time_base
                    .scale(track.track_timestamp_scale)
                    .ok_or(Error::DecodeError("mkv: track timebase is invalid"))?,
            };

            let sample_rate = sample_rate.map(NonZero::get);

            let codec_delay_samples =
                sample_rate.map(|sr| track.codec_delay.into_samples(sr)).unwrap_or(0);

            // The number of bytes in a frame of PCM audio.
            let pcm_frame_bytes = match (&track.audio, track.codec_id.starts_with("A_PCM/")) {
                (Some(audio), true) => audio
                    .bit_depth
                    .and_then(|bits| bits.get().div_ceil(8).checked_mul(audio.channels.get()))
                    .and_then(|bytes| u32::try_from(bytes).ok())
                    .and_then(NonZero::new),
                _ => None,
            };

            // The constant number of frames in a block of a lossless codec that is not
            // otherwise described by the container.
            let fixed_block_samples =
                fixed_block_samples(&track.codec_id, track.codec_private.as_deref());

            // The codecs whose packets carry their duration have an exact timeline.
            let exact = sample_rate.and_then(|_| {
                PacketDurations::new(&track.codec_id, track.codec_private.as_deref())
            });

            // Create the track state.
            let state = TrackState {
                // TODO: This should be 64-bit, but track IDs are 32-bit.
                track_num: u32::try_from(track.number.get())
                    .map_err(|_| Error::Unsupported("mkv: track number too large (report this)"))?,
                default_frame_duration: track.default_duration,
                codec_delay: track.codec_delay,
                track_time_base,
                timestamp_scale: info.timestamp_scale.get(),
                track_timestamp_scale: track.track_timestamp_scale,
                seek_pre_roll: if track.seek_pre_roll.get() > 0 {
                    track.seek_pre_roll
                }
                else {
                    default_seek_pre_roll(&track.codec_id)
                },
                is_opus: track.codec_id == "A_OPUS",
                silk_seen: false,
                pts_tolerance: nanos_to_ticks(
                    i128::from(info.timestamp_scale.get()),
                    track_time_base,
                )
                .map(|ticks| ticks.unsigned_abs().saturating_add(1))
                .unwrap_or(0),
                grid: None,
                exact,
                sample_rate,
                codec_delay_samples,
                pcm_frame_bytes,
                fixed_block_samples,
            };

            // Create the track.
            let mut tr = Track::new(state.track_num);

            tr.with_time_base(track_time_base);

            if let Some(lang_bcp47) = &track.lang_bcp47 {
                tr.with_language(lang_bcp47);
            }
            else {
                tr.with_language(&track.lang);
            }

            tr.with_flags(track.flags);

            if state.codec_delay_samples > 0 {
                tr.with_delay(u32::try_from(state.codec_delay_samples).unwrap_or(u32::MAX));
            }

            if let Some(codec_params) = make_track_codec_params(track)? {
                tr.with_codec_params(codec_params);
            }

            tracks.push(tr);
            track_states.insert(state.track_num, state);
        }

        // Populate media information.
        let mut media_info = MediaInfo::new();

        media_info.with_time_base(time_base);

        if let Some(duration) = info.duration {
            media_info.with_duration(Duration::new(duration.get().round() as u64));
        }

        let first_cluster_pos = current_cluster.map(|cluster| cluster.start);

        let mut reader = Self {
            iter: it,
            media_info,
            tracks,
            track_states,
            attachments,
            chapters,
            metadata,
            cues,
            current_cluster,
            first_cluster_pos,
            is_seekable,
            frames: VecDeque::new(),
            last_pts_end: HashMap::new(),
            cursors: HashMap::new(),
            timeline_index: HashMap::new(),
            timeline_indexed: false,
            indexing: false,
        };

        reader.reset_cursors_to_start();
        reader.prime_pts_grids()?;

        Ok(reader)
    }

    /// For each audio track with a constant frame duration, learn the grid of timestamps at which
    /// its blocks start (see `TrackState::snap_pts`) from the first block of the track.
    ///
    /// Reads the first few blocks of the stream, then rewinds to the first cluster. Does nothing
    /// if the stream is not seekable.
    fn prime_pts_grids(&mut self) -> Result<()> {
        // Maximum number of elements to read looking for the first block of all audio tracks.
        const MAX_ELEMENTS: usize = 256;

        if !self.is_seekable {
            return Ok(());
        }

        let num_audio_tracks =
            self.track_states.values().filter(|state| state.sample_rate.is_some()).count();

        if num_audio_tracks == 0 {
            return Ok(());
        }
        // The timestamp and duration of the first frame of each track.
        let mut first_frames = HashMap::new();

        for _ in 0..MAX_ELEMENTS {
            for frame in self.frames.drain(..) {
                let is_audio = self
                    .track_states
                    .get(&frame.track_num)
                    .is_some_and(|state| state.sample_rate.is_some());

                if is_audio {
                    first_frames
                        .entry(frame.track_num)
                        .or_insert((frame.pts.get(), frame.dur.get()));
                }
            }

            if first_frames.len() >= num_audio_tracks {
                break;
            }

            // If the stream cannot be read (or has ended), do the best with what has been read.
            if !matches!(self.next_element(), Ok(true)) {
                break;
            }
        }

        self.rewind_to_first_cluster()?;

        for (track_num, (anchor, dur)) in first_frames {
            let Some(state) = self.track_states.get_mut(&track_num)
            else {
                continue;
            };

            // The blocks of a track with an exact timeline are not expected to be on a grid: the
            // duration of the packets varies, and a default duration is only an approximation.
            if state.sample_rate.is_none() || state.exact.is_some() {
                continue;
            }

            // The frame duration is the default frame duration, if the track has one. PCM audio
            // has no default frame duration, but the size of its blocks is constant.
            let step = match state.default_frame_duration {
                Some(default_dur) => {
                    nanos_to_ticks(i128::from(default_dur.get()), state.track_time_base)
                        .map(i64::unsigned_abs)
                }
                None if state.pcm_frame_bytes.is_some() => Some(dur),
                None => state.fixed_block_samples,
            };

            if let Some(step) = step.filter(|&step| step > 0) {
                state.grid = Some(PtsGrid { anchor, step });
            }
        }

        Ok(())
    }

    /// Discard all queued frames and reposition the iterator to the start of the first Cluster.
    fn rewind_to_first_cluster(&mut self) -> Result<()> {
        // Ascend back to the segment element.
        self.iter.pop_elements_upto(MkvElement::Segment)?;
        // Seek to the first cluster (or, if it is not known, the start of the segment).
        self.iter.seek_to_child(self.first_cluster_pos.unwrap_or(0))?;

        self.frames.clear();
        self.reset_cursors_to_start();
        self.current_cluster = None;
        Ok(())
    }

    /// Discard all queued frames and reposition the iterator to the cluster of a cue point.
    fn seek_to_cue(&mut self, cluster_pos: u64, cluster_rel_pos: Option<u64>) -> Result<()> {
        // Ascend back to the segment element.
        self.iter.pop_elements_upto(MkvElement::Segment)?;

        // Seek to the specific cluster element.
        self.iter.seek_to_child(cluster_pos)?;

        self.frames.clear();
        self.cursors.clear();
        self.current_cluster = None;

        // Resume iteration.
        let start = match self.iter.next_header()? {
            // The seeked element is a cluster.
            Some(header) if header.element_type() == MkvElement::Cluster => header.pos(),
            // The seeked element is not a cluster or there were no more elements at the cue
            // position. The cue point was malformed.
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        // Descend into the cluster element.
        self.iter.push_element()?;

        // Do not trust the cue's timestamp to be the timestamp of the cluster (a cue point carries
        // the timestamp of the referenced block, not the cluster's). Read the actual cluster
        // timestamp.
        self.current_cluster = Some(ClusterState { timestamp: None, start });

        while self.current_cluster.is_some_and(|cluster| cluster.timestamp.is_none())
            && self.frames.is_empty()
        {
            if !self.next_element()? {
                break;
            }
        }

        // If a cluster relative position is available, and it is ahead of the current position,
        // seek to the exact simple block or block group element.
        if let (Some(cluster_rel_pos), Some(cluster)) = (cluster_rel_pos, self.iter.parent()) {
            if cluster.element_type() == MkvElement::Cluster
                && self.iter.pos() <= cluster.data_pos().saturating_add(cluster_rel_pos)
            {
                self.iter.seek_to_child(cluster_rel_pos)?;
            }
        }

        // The position in the stream is no longer known.
        self.cursors.clear();

        Ok(())
    }

    fn seek_track_by_ts_forward(
        &mut self,
        track_id: u32,
        target_ts: Timestamp,
        required_ts: Timestamp,
    ) -> Result<SeekedTo> {
        let actual_ts = loop {
            // Frames of other tracks are of no interest.
            self.frames.retain(|frame| frame.track_num == track_id);

            // The frame to seek to is the last frame that starts at, or before, the target. Any
            // frame that is followed by another frame that also starts at, or before, the target
            // is skipped.
            while self.frames.len() >= 2 && self.frames[1].pts.into_ts() <= target_ts {
                self.frames.pop_front();
            }

            if self.frames.len() >= 2 {
                break self.frames[0].pts.into_ts();
            }

            if !self.next_element()? {
                // There are no more elements. The remaining frame, if any, is the last frame of
                // the track and is only a valid seek target if it contains the target.
                if let Some(frame) = self.frames.front() {
                    let end = frame
                        .pts
                        .into_ts()
                        .checked_add(frame.dur.into_dur())
                        .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                    if end >= target_ts {
                        break frame.pts.into_ts();
                    }
                }
                return seek_error(SeekErrorKind::OutOfRange);
            }
        };

        Ok(SeekedTo { track_id, required_ts, actual_ts })
    }

    fn seek_track_by_ts_atomic(
        &mut self,
        id: u32,
        tb: TimeBase,
        ts: Timestamp,
    ) -> Result<SeekedTo> {
        // Save the iterator, cluster, and frame queue states to restore in-case of and error.
        let iter_state = self.iter.save_state();
        let cluster_state = self.current_cluster;
        let frames = std::mem::take(&mut self.frames);
        let cursors = self.cursors.clone();

        let mut result = self.seek_track_by_ts(id, tb, ts);

        // Reading the frames of the seek may reveal that an Opus track contains SILK frames,
        // which need a longer pre-roll than a CELT-only track. If so, seek again.
        if result.is_ok() {
            if let Some(state) = self.track_states.get_mut(&id) {
                let before = state.effective_seek_pre_roll();

                if state.is_opus
                    && !state.silk_seen
                    && self.frames.iter().any(|frame| {
                        frame.track_num == id
                            && frame.data.first().is_some_and(|&b| opus::toc_has_silk(b))
                    })
                {
                    state.silk_seen = true;
                }

                if state.effective_seek_pre_roll() > before && self.is_seekable {
                    log::debug!("seek: pre-roll grew, seeking again");
                    result = self.seek_track_by_ts(id, tb, ts);
                }
            }
        }

        match result {
            Err(err) => {
                // Restore saved iterator, cluster, and frame queue states.
                self.iter.restore_state(iter_state)?;
                self.current_cluster = cluster_state;
                self.frames = frames;
                self.cursors = cursors;
                Err(err)
            }
            Ok(seeked) => {
                // The first packet read after a seek has no predecessor.
                self.last_pts_end.clear();
                Ok(seeked)
            }
        }
    }

    fn seek_track_by_ts(&mut self, id: u32, tb: TimeBase, ts: Timestamp) -> Result<SeekedTo> {
        log::debug!("seeking track_id={id} to ts={ts}");

        self.ensure_timeline_index(id)?;

        let state =
            self.track_states.get(&id).ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?;

        // Matroska/WebM signals a codec-specific `SeekPreRoll` (in Matroska ticks). RFC 7845
        // section 4.6 mandates at least 80ms/3840 samples for Opus: after a `reset`, the decoder
        // must decode (and discard) that much audio before its internal state (SILK LPC/LTP
        // history, CELT MDCT overlap, post-filter memory) has re-converged. For Opus, a longer
        // pre-roll is used (see `TrackState::effective_seek_pre_roll`). Back the seek
        // target off by this amount so both the cue lookup and the forward frame scan below land
        // on an earlier packet; `actual_ts` in the returned `SeekedTo` will be <= `required_ts`
        // and the caller is expected to decode-and-discard the difference — the same contract
        // `symphonia-format-ogg` uses for Vorbis's/Opus's own pre-roll via `max_rap_period`.
        let target_ts =
            ts.saturating_sub(state.effective_seek_pre_roll().into_track_ticks(tb).into_dur());

        // Cue points carry the raw timestamp of the block they reference, whereas the timestamps
        // of frames (and therefore the seek target) are shifted back by the codec delay. Shift the
        // target forward again when searching for cue points, and convert it from Track ticks to
        // Segment ticks (the unit of cue point timestamps).
        let nanos =
            ticks_to_nanos(target_ts.get(), tb).saturating_add(i128::from(state.codec_delay.get()));

        let cue_ts = if nanos < 0 {
            0
        }
        else if state.track_timestamp_scale == 1.0 {
            u64::try_from(nanos / i128::from(state.timestamp_scale)).unwrap_or(u64::MAX)
        }
        else {
            (nanos as f64 / (state.timestamp_scale as f64 * state.track_timestamp_scale)).floor()
                as u64
        };

        // Find the candidate cue points (in descending order of preference): all cue points of the
        // track that are at, or before, the target.
        //
        // A cue point also carries the position of a block within its cluster. That position is
        // only meaningful for the track the cue point is for: a muxer may write cue points for
        // another track only (e.g., the keyframes of a video track in a file with audio), and such
        // a position can point past the first block of the track being seeked.
        let mut candidates = Vec::new();

        if let Some(cues) = &self.cues {
            // If the track has no cue points at all, fallback to using all cue points.
            let track_has_cues =
                cues.points.iter().any(|point| point.positions.track.get() == u64::from(id));

            candidates.extend(
                cues.points
                    .iter()
                    .take_while(|point| point.time.get() <= cue_ts)
                    .filter(|point| !track_has_cues || point.positions.track.get() == u64::from(id))
                    .map(|point| {
                        let rel_pos = point
                            .positions
                            .cluster_rel_pos
                            .filter(|_| point.positions.track.get() == u64::from(id));
                        (point.positions.cluster_pos, rel_pos)
                    }),
            );
        }

        log::debug!("found {} candidate cue points", candidates.len());

        // The timestamp of a cue point may not be exactly that of the first frame it refers to, so
        // the frame found by the forward scan may be after the target. Fallback to earlier cue
        // points in such a case.
        const MAX_CUE_ATTEMPTS: usize = 4;

        let mut seeked: Option<SeekedTo> = None;

        'cues: for &(cluster_pos, cluster_rel_pos) in candidates.iter().rev().take(MAX_CUE_ATTEMPTS)
        {
            // If the position of the block within the cluster led to a frame after the target
            // (the position is wrong), retry from the start of the cluster.
            let positions: &[Option<u64>] =
                if cluster_rel_pos.is_some() { &[cluster_rel_pos, None] } else { &[None] };

            for &rel_pos in positions {
                self.seek_to_cue(cluster_pos, rel_pos)?;

                let attempt = self.seek_track_by_ts_forward(id, target_ts, ts)?;
                let is_early = attempt.actual_ts <= target_ts;
                seeked = Some(attempt);

                if is_early {
                    break 'cues;
                }
            }
        }

        if let Some(seeked) = seeked {
            if seeked.actual_ts <= target_ts || !self.is_seekable {
                return Ok(seeked);
            }
            // None of the cue points led to a frame at or before the target (the cue points are
            // not usable). Scan from the start of the stream instead.
        }

        // If the stream is seekable, scan from the first cluster. Otherwise, it is only possible to
        // scan forward from the current position.
        if self.is_seekable {
            self.rewind_to_first_cluster()?;
        }

        self.seek_track_by_ts_forward(id, target_ts, ts)
    }

    /// Peek at the track number of the current simple block, and return true if the track has an
    /// exact timeline. If the track cannot be determined, returns true.
    fn is_block_of_timeline_track(&mut self) -> bool {
        let mut head = [0u8; 8];

        let Ok(len) = self.iter.peek_binary(&mut head)
        else {
            return true;
        };

        match read_unsigned_vint(&mut BufReader::new(&head[..len])) {
            Ok(track_num) => u32::try_from(track_num)
                .ok()
                .and_then(|track_num| self.track_states.get(&track_num))
                .is_none_or(|state| state.exact.is_some()),
            Err(_) => true,
        }
    }

    /// Reset the timelines of all tracks that have one to the start of the stream.
    fn reset_cursors_to_start(&mut self) {
        self.cursors.clear();

        for (&track_num, state) in &self.track_states {
            if state.exact.is_some() {
                self.cursors.insert(track_num, Cursor::START);
            }
        }
    }

    /// Scan the stream once to record the exact position of every block of the tracks that have
    /// an exact timeline, if a track needs it.
    ///
    /// The exact position of a Vorbis packet is the sum of the durations of all the packets before
    /// it, so it cannot be known after a jump to a random position in the stream (e.g., by a
    /// cue point) without having read everything before. The timestamp of the block, being only
    /// precise to the timestamp scale, cannot be used to recover it. The index is used to resume
    /// the exact timeline at the first block read after a jump.
    fn ensure_timeline_index(&mut self, track_id: u32) -> Result<()> {
        if self.timeline_indexed || !self.is_seekable {
            return Ok(());
        }

        if !self.track_states.get(&track_id).is_some_and(TrackState::needs_timeline_index) {
            return Ok(());
        }

        // Only attempt to scan the stream once.
        self.timeline_indexed = true;

        log::debug!("scanning the stream to index the timeline");

        let iter_state = self.iter.save_state();
        let cluster = self.current_cluster;
        let frames = std::mem::take(&mut self.frames);
        let cursors = std::mem::take(&mut self.cursors);

        self.timeline_index.clear();
        self.indexing = true;

        let scan = self.rewind_to_first_cluster().and_then(|_| {
            while self.next_element()? {
                // Only the effect of reading the elements on the timeline is of interest.
                self.frames.clear();
            }
            Ok(())
        });

        self.indexing = false;

        if let Err(err) = scan {
            // The part of the stream that was scanned is still indexed.
            warn!("failed to scan the stream to index the timeline ({err})");
        }

        // Return to where the stream was.
        self.iter.restore_state(iter_state)?;
        self.current_cluster = cluster;
        self.frames = frames;
        self.cursors = cursors;
        Ok(())
    }

    /// Give the frames of a block, that was just read at `block_pos` and starts at index
    /// `first_frame` of the frame queue, their exact timestamps and durations if their track has
    /// an exact timeline.
    fn apply_exact_timeline(&mut self, block_pos: u64, first_frame: usize) {
        let Some(track_num) = self.frames.get(first_frame).map(|frame| frame.track_num)
        else {
            return;
        };

        let Some(state) = self.track_states.get(&track_num)
        else {
            return;
        };

        let Some(exact) = state.exact.as_ref()
        else {
            return;
        };

        let tolerance = state.pts_tolerance;

        // Resume from the previous block of the track, or, after a jump, from the index.
        let mut cursor = self.cursors.remove(&track_num).unwrap_or_else(|| {
            self.timeline_index
                .get(&track_num)
                .and_then(|index| {
                    index.binary_search_by_key(&block_pos, |entry| entry.0).ok().map(|i| index[i].1)
                })
                .unwrap_or(Cursor::UNKNOWN)
        });

        // The timestamp of a block is only precise to the timestamp scale, but a block that does
        // not start where the timeline is by more than that means the timeline was broken (e.g.,
        // by missing blocks). Anchor it to the block again.
        let block_pts = self.frames[first_frame].pts.get();
        let max_gap = tolerance.saturating_add(exact.timestamp_slack());

        if cursor.next_pts.is_some_and(|next| next.abs_diff(block_pts) > max_gap) {
            log::debug!("timeline of track {track_num} is not contiguous, re-anchoring");
            cursor.next_pts = None;
        }
        else if cursor.approx {
            // The position of the block is only a guess. The timestamp of the block is not.
            cursor.next_pts = None;
        }
        cursor.approx = false;

        if self.indexing {
            self.timeline_index.entry(track_num).or_default().push((block_pos, cursor));
        }

        for frame in self.frames.iter_mut().skip(first_frame) {
            if frame.track_num != track_num {
                continue;
            }

            let Some(timing) = exact.advance(&mut cursor.prev, &frame.data)
            else {
                continue;
            };

            let pts = match cursor.next_pts {
                Some(next) => next,
                None => {
                    let block_pts = frame.pts.get();

                    if timing.is_start {
                        // The first packet of a Vorbis stream decodes to nothing; the audio starts
                        // with the second. A stream that starts at 0 either timestamps the first
                        // packet with the time its audio would have started at had it been
                        // decoded (ffmpeg), or with 0 (mkvmerge). In both cases, the audio
                        // starts at 0 exactly, and not only to within the timestamp scale.
                        let lead = i64::try_from(timing.lead).unwrap_or(0);

                        if block_pts.abs_diff(-lead) <= tolerance
                            || block_pts.abs_diff(0) <= tolerance
                        {
                            -lead
                        }
                        else {
                            block_pts
                        }
                    }
                    else {
                        block_pts
                    }
                }
            };

            frame.pts = SignedTrackTicks::from(pts);
            frame.trim_start = frame.trim_start.saturating_add(timing.lead);
            frame.dur = TrackTicks::from(
                timing.dur.saturating_sub(frame.trim_start).saturating_sub(frame.trim_end),
            );

            // The next packet starts where the decoded frames of this packet end.
            cursor.next_pts = Some(pts.saturating_add_unsigned(timing.dur));
            cursor.approx = timing.approx;
        }

        self.cursors.insert(track_num, cursor);
    }

    fn next_element(&mut self) -> Result<bool> {
        match self.read_next_element() {
            // The stream ended inside an element of unknown size (e.g., live streams): the end of
            // the stream is the end of the element and not an error.
            Err(Error::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof
                    && self.iter.has_unknown_size_ancestor() =>
            {
                log::debug!("end of stream reached inside an element of unknown size");
                Ok(false)
            }
            result => result,
        }
    }

    fn read_next_element(&mut self) -> Result<bool> {
        match self.iter.next_header()? {
            None => {
                // The EBML iterator has consumed all child elements at the current level of the
                // document.
                match self.iter.parent() {
                    None => {
                        // The parent is the document. The media has ended.
                        return Ok(false);
                    }
                    Some(parent) if parent.element_type() == MkvElement::Cluster => {
                        // The parent was a cluster element. Reset the cluster state.
                        self.current_cluster = None;
                    }
                    Some(parent) if parent.element_type() == MkvElement::Segment => {
                        // The parent was the segment element. The media has ended. Do not ascend
                        // out of the segment element so that seeking within it remains possible.
                        return Ok(false);
                    }
                    _ => (),
                }

                // Ascend to its parent.
                self.iter.pop_element()?;
            }
            Some(child) => {
                let child_pos = child.pos();

                match child.element_type() {
                    // Cluster element.
                    MkvElement::Cluster => {
                        self.current_cluster =
                            Some(ClusterState { timestamp: None, start: child.pos() });

                        // Descend into the cluster.
                        self.iter.push_element()?;
                    }
                    // Children of a cluster element.
                    MkvElement::Timestamp => {
                        // Cluster timestamp element.
                        match self.current_cluster.as_mut() {
                            Some(cc) => {
                                cc.timestamp = self.iter.read_u64()?.map(SegmentTicks::from)
                            }
                            _ => log::warn!("expected to have cluster"),
                        }
                    }
                    block_type @ (MkvElement::SimpleBlock | MkvElement::BlockGroup) => {
                        // When scanning the stream, there is no need to read the blocks of tracks
                        // without a timeline (e.g., video).
                        if self.indexing
                            && block_type == MkvElement::SimpleBlock
                            && !self.is_block_of_timeline_track()
                        {
                            self.iter.skip_data()?;
                            return Ok(true);
                        }

                        // Get the current cluster information.
                        let Some(cluster) = self.current_cluster.as_ref()
                        else {
                            log::warn!("expected to have cluster");
                            return Ok(true);
                        };

                        // Get the cluster timestamp.
                        let Some(cluster_ts) = cluster.timestamp
                        else {
                            log::warn!("missing cluster timestamp");
                            return Ok(true);
                        };

                        // Get block data, duration, and discard padding.
                        let (data, duration, discard_padding) = match block_type {
                            MkvElement::SimpleBlock => (self.iter.read_binary()?, None, None),
                            MkvElement::BlockGroup => {
                                let group = self.iter.read_master_element::<BlockGroupElement>()?;
                                (group.data, group.duration, group.discard_padding)
                            }
                            _ => unreachable!(),
                        };

                        let first_frame = self.frames.len();

                        // Extract frames.
                        if !extract_frames(
                            &data,
                            duration,
                            discard_padding,
                            cluster_ts,
                            &self.track_states,
                            &mut self.frames,
                        )? {
                            warn!("pts for block is too large");
                            return Ok(false);
                        }
                        self.apply_exact_timeline(child_pos, first_frame);
                    }
                    // All other elements.
                    other => {
                        log::debug!("ignored element {other:?}");
                    }
                }
            }
        }

        Ok(true)
    }
}

impl ProbeableFormat<'_> for MkvReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(MkvReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MKV_FORMAT_INFO,
            &["webm", "mkv"],
            &["video/webm", "video/x-matroska"],
            &[b"\x1A\x45\xDF\xA3"] // Top-level element Ebml element
        )]
    }
}

impl FormatReader for MkvReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MKV_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Time { time, track_id } => {
                let track = match track_id {
                    Some(id) => self.tracks.iter().find(|track| track.id == id),
                    None => self.tracks.first(),
                };
                let track = track.ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?;
                let tb = track.time_base.expect("track always has a timebase");
                let ts = match tb.calc_timestamp(time) {
                    Some(ts) => ts,
                    None => {
                        warn!("seek ts is too large");
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                };
                let track_id = track.id;
                self.seek_track_by_ts_atomic(track_id, tb, ts)
            }
            SeekTo::Timestamp { ts, track_id } => {
                match self.tracks.iter().find(|t| t.id == track_id) {
                    Some(track) => {
                        let tb = track.time_base.expect("track always has a timebase");
                        self.seek_track_by_ts_atomic(track_id, tb, ts)
                    }
                    None => seek_error(SeekErrorKind::InvalidTrack),
                }
            }
        }
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                let mut pts = frame.pts;
                let mut dur = frame.dur.get();

                // Samples to be discarded from the start of the packet: first, any negative
                // `DiscardPadding` of the block.
                let mut trim_start = frame.trim_start;

                // Remember if an Opus track contains SILK, which needs a longer seek pre-roll.
                if let Some(state) = self.track_states.get_mut(&frame.track_num) {
                    if state.is_opus
                        && !state.silk_seen
                        && frame.data.first().is_some_and(|&b| opus::toc_has_silk(b))
                    {
                        state.silk_seen = true;
                    }
                }

                if let Some(state) = self.track_states.get(&frame.track_num) {
                    // Block timestamps are only as precise as the segment's timestamp scale
                    // (usually 1ms). If a packet starts where the previous one of the track ended,
                    // to within the precision of the timestamp, then it is contiguous. Use the end
                    // of the previous packet as its start so that the timeline of audio tracks
                    // is sample accurate.
                    if state.sample_rate.is_some() {
                        if let Some(&end) = self.last_pts_end.get(&frame.track_num) {
                            if end.abs_diff(pts.get()) <= state.pts_tolerance {
                                pts = SignedTrackTicks::from(end);
                            }
                        }
                    }

                    // Second, the RFC 7845-section-4.2-equivalent gapless trim: `codec_delay`
                    // priming samples are still present in the decoded output and must be
                    // discarded, mirroring `symphonia-format-ogg`'s Opus `pre_skip` handling.
                    // The timestamp is already shifted by `codec_delay` (see
                    // `calculate_block_pts`), so it is negative exactly for frames still within
                    // the delay region.
                    //
                    // This is computed from the packet's timestamp alone, and not from a running
                    // counter, so that the exact same trim is applied to the start of the stream
                    // no matter how many times (or after which seeks) it is reached.
                    let delay_trim = state.codec_delay_trim(pts, frame.dur);

                    trim_start = trim_start.saturating_add(delay_trim);
                    // The duration of a packet only includes the frames that are not trimmed.
                    dur = dur.saturating_sub(delay_trim);
                }

                let pts = pts.get();

                let mut packet = Packet::new(
                    frame.track_num,
                    Timestamp::new(pts),
                    Duration::new(dur),
                    frame.data,
                );

                packet.trim_start = Duration::new(trim_start);
                packet.trim_end = Duration::new(frame.trim_end);

                // The next packet is expected to start where the decoded frames of this packet
                // end.
                if let Some(end) = pts.checked_add_unsigned(packet.block_dur().get()) {
                    self.last_pts_end.insert(frame.track_num, end);
                }

                return Ok(Some(packet));
            }

            if !self.next_element()? {
                // Reached the end of stream.
                return Ok(None);
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.iter.into_inner()
    }
}

impl Scoreable for MkvReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ReadEbml for MediaSourceStream<'_> {}

impl From<EbmlError> for Error {
    fn from(value: EbmlError) -> Self {
        // All non-IO EBML errors are mapped to a decode error.
        let msg = match value {
            EbmlError::IoError(err) => return Error::IoError(err),
            EbmlError::InvalidEbmlElementIdLength => "mkv (ebml): invalid ebml element id length",
            EbmlError::InvalidEbmlDataLength => "mkv (ebml): invalid ebml vint length",
            EbmlError::UnknownElement => "mkv (ebml): the element is unknown",
            EbmlError::UnknownElementDataSize => "mkv (ebml): the element data size is unknown",
            EbmlError::UnexpectedElement => "mkv (ebml): encountered an unexpected element",
            EbmlError::UnexpectedElementDataType => {
                "mkv (ebml): unexpected element data type for the operation"
            }
            EbmlError::UnexpectedElementDataSize => {
                "mkv (ebml): unexpected data size for the element's data type"
            }
            EbmlError::NoElement => "mkv (ebml): no current element",
            EbmlError::NoParent => "mkv (ebml): no parent element",
            EbmlError::NotAnAncestor => {
                "mkv (ebml): the element is not an ancestor of the current element"
            }
            EbmlError::Overrun => "mkv (ebml): the element was overrun when read",
            EbmlError::ExpectedMasterElement => "mkv (ebml): expected a master element",
            EbmlError::ExpectedNonMasterElement => "mkv (ebml): expected a non-master element",
            EbmlError::SeekOutOfRange => "mkv (ebml): the seek was out of range",
            EbmlError::BufferTooSmall => "mkv (ebml): the buffer is too small",
            EbmlError::MaximumDepthReached => "mkv (ebml): maximum ebml document depth reached",
            EbmlError::ElementError(err) => err,
        };
        Error::DecodeError(msg)
    }
}
