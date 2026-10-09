// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Elementary stream (ES) framing for audio carried in MPEG PES packets: the parsers that split the
//! payload of PES packets into codec frames, and [`EsTrack`], which assigns timestamps to them.
//!
//! These are shared by the MPEG program stream and transport stream demuxers.

use std::collections::VecDeque;

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use super::audio::latm::{StreamMuxConfig, read_audio_mux_element};
use super::audio::mpa::{self, MpaFramer};
use super::audio::{
    AAC_SEEK_MAX_PREROLL_FRAMES, AudioSpecificConfig,
    MAX_IMPLICIT_SBR_PROBE_BLOCKS, Mpeg4AudioChannels, Mpeg4AudioSampleRate, aac_overlap_frames,
    aac_sbr_phase_period, aac_seek_start_frame, aac_seek_start_frame_with_period,
    get_audio_codec_profile, get_mpeg4_audio_channels_by_config_index,
    get_mpeg4_audio_sample_rate_by_index, may_have_implicit_sbr,
};
use super::timeline::Timeline;

/// The number of samples in an AAC frame.
const AAC_FRAME_SAMPLES: u64 = 1024;

/// Looks for implicit SBR (and parametric stereo) in the `raw_data_block()`s `blocks` of an AAC-LC
/// stream with the audio specific config `asc`, which cannot signal them. Returns the audio specific
/// config that signals the extension explicitly if it is found.
///
/// Detecting it requires decoding the blocks, so it is provided by the AAC codec crate
/// (`symphonia_codec_aac::detect_implicit_sbr`), which this crate cannot depend on.
pub type ImplicitSbrDetector = fn(asc: &[u8], blocks: &[&[u8]]) -> Option<Box<[u8]>>;

/// The timeline and seek parameters of an AAC stream. Like the readers of the AAC crate, the
/// timeline of the readers is in decoded frames (at the output sample rate of the stream, which is
/// twice the core rate if the stream has SBR).
#[derive(Copy, Clone, Debug)]
pub struct AacTimeline {
    frame_dur: u64,
    sbr: bool,
    overlap: u64,
    period: u64,
}

impl Default for AacTimeline {
    /// The timeline of AAC-LC without SBR.
    fn default() -> Self {
        AacTimeline { frame_dur: AAC_FRAME_SAMPLES, sbr: false, overlap: 1, period: 16 }
    }
}

impl AacTimeline {
    /// The timeline of the stream with the audio specific config `asc`.
    pub fn from_config(asc: &AudioSpecificConfig) -> Self {
        let ratio = u64::from(asc.output_sample_rate() / asc.sample_rate.max(1)).max(1);

        AacTimeline {
            frame_dur: (asc.samples as u64).max(1) * ratio,
            sbr: asc.sbr_present,
            overlap: aac_overlap_frames(asc.object_type),
            period: aac_sbr_phase_period(asc),
        }
    }

    /// The duration of a frame (access unit) in decoded frames.
    pub fn frame_dur(&self) -> u64 {
        self.frame_dur
    }

    /// Given a timestamp to seek to, get the timestamp to start decoding at, so that a decoder
    /// will reproduce a continuous decode from the timestamp onwards.
    pub fn seek_target(&self, target: i64) -> i64 {
        let frame = u64::try_from(target).unwrap_or(0) / self.frame_dur;
        let start =
            aac_seek_start_frame_with_period(frame, self.sbr, self.overlap, self.period).min(frame);
        (start * self.frame_dur) as i64
    }
}

/// The stream information of an AAC stream with the audio specific config `asc`, whose bytes are
/// `extra_data`: the codec parameters describe the decoded output, and the timeline is in decoded
/// frames.
pub fn aac_es_info(asc: &AudioSpecificConfig, extra_data: &[u8]) -> EsInfo {
    let mut params = AudioCodecParameters::new();

    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_extra_data(extra_data.into());

    if let Some(channels) = asc.output_channels() {
        params.with_channels(channels);
    }

    if let Some(profile) = get_audio_codec_profile(asc) {
        params.with_profile(profile);
    }

    EsInfo { params, rate: asc.output_sample_rate() }
}

/// The pre-roll, in samples at 48 kHz, of an Opus stream (RFC 7845 §4.6: at least 80 ms).
const OPUS_SEEK_PREROLL: i64 = 80 * 48;

/// The stream information determined from the elementary stream.
#[derive(Clone, Debug)]
pub struct EsInfo {
    /// The codec parameters of the stream.
    pub params: AudioCodecParameters,
    /// The sample rate in Hz of the timeline of the stream: `1 / rate` is the timebase. For
    /// streams with SBR this is the core sample rate, not the output sample rate.
    pub rate: u32,
}

/// A frame (or access unit) of an elementary stream.
#[derive(Clone, Debug)]
pub struct EsFrame {
    /// The offset in the elementary stream, counting from the first byte ever pushed to the
    /// parser, of the first byte of the frame.
    pub start: u64,
    /// The frame data, as it should be given to the decoder.
    pub data: Box<[u8]>,
    /// The duration of the frame in ticks of the stream's timebase, excluding trimmed samples.
    pub dur: u64,
    /// The number of ticks to trim from the start of the decoded frame.
    pub trim_start: u64,
    /// The number of ticks to trim from the end of the decoded frame.
    pub trim_end: u64,
}

/// An `EsParser` splits an elementary stream into frames.
pub trait EsParser: Send + Sync {
    /// Append data of the elementary stream. For parsers that do not synchronise to frames, each
    /// call must contain whole frames (i.e., the payload of a PES packet).
    fn push(&mut self, data: &[u8]);

    /// Get the next frame, if any. If `flush` is true, no more data will be pushed before the
    /// next call.
    fn next_frame(&mut self, flush: bool) -> Option<EsFrame>;

    /// Discard all buffered data and synchronisation state (after a seek).
    fn clear(&mut self);

    /// The stream information. `None` until enough of the stream has been parsed.
    fn info(&self) -> Option<&EsInfo>;

    /// Given a timestamp to seek to, get the timestamp to start decoding at, so that the decoder
    /// will reproduce a continuous decode from the timestamp onwards.
    fn seek_start_ts(&self, target: i64) -> i64;

    /// A clone of this parser in its initial state, for measuring frame durations.
    fn fresh(&self) -> Box<dyn EsParser>;

    /// If all frames of the stream have the same duration, that duration in ticks, otherwise 0.
    fn frame_grid(&self) -> u64 {
        0
    }
}

/// An MPEG audio (layer 1, 2, or 3) elementary stream parser.
#[derive(Default)]
pub struct MpaEs {
    framer: MpaFramer,
    info: Option<EsInfo>,
    frame_len: u64,
    layer: u8,
    pushed: u64,
}

impl MpaEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for MpaEs {
    fn push(&mut self, data: &[u8]) {
        self.framer.push(data);
        self.pushed += data.len() as u64;
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            let frame = self.framer.next_frame(flush)?;

            // Skip Xing/Info/VBRI frames.
            if frame.info.is_info_frame(&frame.data) {
                continue;
            }

            if self.info.is_none() {
                let mut params = AudioCodecParameters::new();
                params
                    .for_codec(frame.info.codec)
                    .with_sample_rate(frame.info.sample_rate)
                    .with_channels(frame.info.channels());

                self.info = Some(EsInfo { params, rate: frame.info.sample_rate });
                self.frame_len = u64::from(frame.info.samples_per_frame);
                self.layer = frame.info.layer;
            }

            return Some(EsFrame {
                start: frame.start,
                data: frame.data.into_boxed_slice(),
                dur: u64::from(frame.info.samples_per_frame),
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn clear(&mut self) {
        self.framer.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        let frames = if self.layer == 3 {
            mpa::MPA_SEEK_PREROLL_FRAMES + mpa::MPA_L3_RESERVOIR_PREROLL_FRAMES
        }
        else {
            mpa::MPA_SEEK_PREROLL_FRAMES
        };

        target - (frames * self.frame_len) as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(MpaEs::new())
    }

    fn frame_grid(&self) -> u64 {
        self.frame_len
    }
}

/// The header of an ADTS frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct AdtsHeader {
    rate: u32,
    channel_config: u32,
    /// The index of the sampling frequency.
    sf_index: u32,
    /// The audio object type.
    profile: u32,
    frame_len: usize,
    header_len: usize,
}

impl AdtsHeader {
    const SIZE_NO_CRC: usize = 7;

    fn parse(b: &[u8]) -> Option<AdtsHeader> {
        if b.len() < Self::SIZE_NO_CRC || b[0] != 0xff || b[1] & 0xf6 != 0xf0 {
            return None;
        }

        let header_len = if b[1] & 1 == 0 { 9 } else { Self::SIZE_NO_CRC };

        let sf_index = u32::from(b[2] >> 2) & 0xf;
        let profile = u32::from(b[2] >> 6) + 1;

        let rate = match get_mpeg4_audio_sample_rate_by_index(sf_index) {
            Mpeg4AudioSampleRate::SampleRate(rate) => rate,
            _ => return None,
        };

        let channel_config = u32::from(b[2] & 1) << 2 | u32::from(b[3] >> 6);

        // A channel configuration of 0 is defined in-band by a program config element, which is
        // not supported.
        if channel_config == 0 {
            return None;
        }

        let frame_len =
            (usize::from(b[3] & 3) << 11) | (usize::from(b[4]) << 3) | usize::from(b[5] >> 5);

        // Only one raw data block per frame is supported.
        if b[6] & 3 != 0 || frame_len <= header_len {
            return None;
        }

        Some(AdtsHeader { rate, channel_config, sf_index, profile, frame_len, header_len })
    }

    fn is_compatible(&self, other: &AdtsHeader) -> bool {
        self.rate == other.rate && self.channel_config == other.channel_config
    }
}

/// An AAC in ADTS elementary stream parser.
#[derive(Default)]
pub struct AdtsEs {
    buf: Vec<u8>,
    base: u64,
    locked: Option<AdtsHeader>,
    info: Option<EsInfo>,
    timeline: AacTimeline,
    detector: Option<ImplicitSbrDetector>,
}

impl AdtsEs {
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a parser that looks for implicit SBR (HE-AAC), which ADTS cannot signal, with
    /// `detector`.
    pub fn with_detector(detector: ImplicitSbrDetector) -> Self {
        AdtsEs { detector: Some(detector), ..Default::default() }
    }

    /// The plain audio specific config of an AAC-LC stream with the parameters of the header.
    fn plain_asc(hdr: &AdtsHeader) -> Box<[u8]> {
        // The object type (5 bits, AAC-LC), sampling frequency index (4), channel configuration
        // (4), and GASpecificConfig (3 bits, all clear).
        let bits = (2u32 << 11) | (hdr.sf_index << 7) | (hdr.channel_config << 3);
        bits.to_be_bytes()[2..].into()
    }

    /// The `raw_data_block()`s of the (up to) `MAX_IMPLICIT_SBR_PROBE_BLOCKS` frames from the
    /// offset `i` of the buffer.
    fn lookahead(&self, mut i: usize, hdr: &AdtsHeader) -> Vec<&[u8]> {
        let mut blocks = Vec::new();

        while blocks.len() < MAX_IMPLICIT_SBR_PROBE_BLOCKS {
            let Some(next) = self.buf.get(i..).and_then(AdtsHeader::parse)
            else {
                break;
            };

            let end = i + next.frame_len;

            if !hdr.is_compatible(&next) || end > self.buf.len() {
                break;
            }

            blocks.push(&self.buf[i + next.header_len..end]);
            i = end;
        }

        blocks
    }
}

impl EsParser for AdtsEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        let mut i = 0;

        let found = loop {
            while i + 1 < self.buf.len() && !(self.buf[i] == 0xff && self.buf[i + 1] & 0xf6 == 0xf0)
            {
                i += 1;
            }

            let Some(hdr) = self.buf.get(i..).and_then(AdtsHeader::parse)
            else {
                if self.buf.len() >= i + Self::MIN_HEADER && self.buf.get(i) == Some(&0xff) {
                    // A bad header at the sync word.
                    i += 1;
                    continue;
                }
                break None;
            };

            if self.locked.is_some_and(|l| !l.is_compatible(&hdr)) {
                i += 1;
                continue;
            }

            let end = i + hdr.frame_len;

            if end > self.buf.len() {
                if flush {
                    i += 1;
                    continue;
                }
                break None;
            }

            if end + Self::MIN_HEADER <= self.buf.len() {
                match AdtsHeader::parse(&self.buf[end..]) {
                    Some(next) if next.is_compatible(&hdr) => (),
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            else if !flush {
                break None;
            }

            break Some((i, hdr, end));
        };

        match found {
            Some((i, hdr, end)) => {
                // HE-AAC cannot be signalled in ADTS: look for SBR and parametric stereo in the
                // first frames, which are waited for.
                let mut explicit = None;

                if self.info.is_none() && hdr.profile == 2 {
                    if let Some(detector) = self.detector {
                        let plain = Self::plain_asc(&hdr);
                        let may = AudioSpecificConfig::read(&plain)
                            .is_ok_and(|asc| may_have_implicit_sbr(&asc));

                        if may {
                            let blocks = self.lookahead(i, &hdr);

                            if blocks.len() < MAX_IMPLICIT_SBR_PROBE_BLOCKS && !flush {
                                return None;
                            }

                            explicit = detector(&plain, &blocks);
                        }
                    }
                }

                let data = self.buf[i + hdr.header_len..end].to_vec().into_boxed_slice();
                let start = self.base + i as u64;
                self.buf.drain(..end);
                self.base += end as u64;
                self.locked = Some(hdr);

                if self.info.is_none() {
                    let channels =
                        match get_mpeg4_audio_channels_by_config_index(hdr.channel_config) {
                            Mpeg4AudioChannels::Channels(channels) => channels,
                            _ => return None,
                        };

                    let sbr_asc = explicit.and_then(|extra_data| {
                        let asc = AudioSpecificConfig::read(&extra_data).ok()?;
                        Some((asc, extra_data))
                    });

                    match sbr_asc {
                        Some((asc, extra_data)) => {
                            self.timeline = AacTimeline::from_config(&asc);
                            self.info = Some(aac_es_info(&asc, &extra_data));
                        }
                        None => {
                            let mut params = AudioCodecParameters::new();
                            params
                                .for_codec(CODEC_ID_AAC)
                                .with_sample_rate(hdr.rate)
                                .with_channels(channels);
                            self.timeline = AacTimeline::default();
                            self.info = Some(EsInfo { params, rate: hdr.rate });
                        }
                    }
                }

                Some(EsFrame {
                    start,
                    data,
                    dur: self.timeline.frame_dur(),
                    trim_start: 0,
                    trim_end: 0,
                })
            }
            None => {
                self.buf.drain(..i);
                self.base += i as u64;
                None
            }
        }
    }

    fn clear(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.locked = None;
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        self.timeline.seek_target(target)
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        // The stream has been probed: the parser does not look for SBR again.
        Box::new(AdtsEs {
            info: self.info.clone(),
            timeline: self.timeline,
            ..Default::default()
        })
    }

    fn frame_grid(&self) -> u64 {
        self.timeline.frame_dur()
    }
}

impl AdtsEs {
    /// The minimum number of bytes required to parse an ADTS header.
    const MIN_HEADER: usize = AdtsHeader::SIZE_NO_CRC;
}

/// Get the timestamp of the AAC frame to start decoding at to decode continuously from `target`.
/// `sbr` is true if the stream may use spectral band replication.
pub fn aac_seek_target(target: i64, sbr: bool) -> i64 {
    let frame = u64::try_from(target).unwrap_or(0) / AAC_FRAME_SAMPLES;
    let start = aac_seek_start_frame(frame, sbr).min(frame);
    debug_assert!(frame - start <= AAC_SEEK_MAX_PREROLL_FRAMES);
    (start * AAC_FRAME_SAMPLES) as i64
}

/// The 11-bit LOAS sync word `0x2b7`.
const LOAS_SYNC: u16 = 0x56e0;
const LOAS_SYNC_MASK: u16 = 0xffe0;
const LOAS_HEADER_LEN: usize = 3;

/// An AAC in LATM elementary stream (with LOAS `AudioSyncStream()` framing) parser.
#[derive(Default)]
pub struct LatmEs {
    buf: Vec<u8>,
    base: u64,
    config: Option<StreamMuxConfig>,
    pending: VecDeque<EsFrame>,
    info: Option<EsInfo>,
    timeline: AacTimeline,
    detector: Option<ImplicitSbrDetector>,
    /// The frames received before the stream mux config, which may be decoded using the config
    /// once it is found.
    early: Vec<(u64, Vec<u8>)>,
}

/// The maximum number of frames kept while waiting for the stream mux config.
const LATM_MAX_EARLY_FRAMES: usize = 32;

impl LatmEs {
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a parser that looks for implicit SBR (HE-AAC with a plain AAC-LC config) with
    /// `detector`.
    pub fn with_detector(detector: ImplicitSbrDetector) -> Self {
        LatmEs { detector: Some(detector), ..Default::default() }
    }

    /// Determine the stream info once the stream mux config is known (and, if the stream may have
    /// implicit SBR, the first frames). Returns false if more frames are required.
    fn finish_info(&mut self, flush: bool) -> bool {
        let Some(config) = &self.config
        else {
            return false;
        };

        let mut asc = config.asc.clone();
        let mut extra_data = config.extra_data.clone();

        if let Some(detector) = self.detector {
            if may_have_implicit_sbr(&asc) {
                if self.pending.len() < MAX_IMPLICIT_SBR_PROBE_BLOCKS && !flush {
                    return false;
                }

                let blocks: Vec<&[u8]> = self
                    .pending
                    .iter()
                    .take(MAX_IMPLICIT_SBR_PROBE_BLOCKS)
                    .map(|frame| &frame.data[..])
                    .collect();

                if let Some(explicit) = detector(&extra_data, &blocks) {
                    if let Ok(explicit_asc) = AudioSpecificConfig::read(&explicit) {
                        asc = explicit_asc;
                        extra_data = explicit;
                    }
                }
            }
        }

        self.timeline = AacTimeline::from_config(&asc);
        self.info = Some(aac_es_info(&asc, &extra_data));
        true
    }

    fn loas_len(b: &[u8]) -> Option<usize> {
        let sync = u16::from_be_bytes([*b.first()?, *b.get(1)?]);

        if sync & LOAS_SYNC_MASK != LOAS_SYNC {
            return None;
        }

        Some(usize::from(sync & 0x1f) << 8 | usize::from(*b.get(2)?))
    }
}

impl EsParser for LatmEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            // The frames wait for the stream info, which gives their duration.
            if self.info.is_none() && self.config.is_some() {
                self.finish_info(flush);
            }

            if self.info.is_some() {
                if let Some(mut frame) = self.pending.pop_front() {
                    frame.dur = self.timeline.frame_dur();
                    return Some(frame);
                }
            }

            let mut i = 0;

            let found = loop {
                while i + 1 < self.buf.len()
                    && !(self.buf[i] == 0x56 && self.buf[i + 1] & 0xe0 == 0xe0)
                {
                    i += 1;
                }

                let Some(len) = Self::loas_len(&self.buf[i..])
                else {
                    break None;
                };

                let end = i + LOAS_HEADER_LEN + len;

                if len == 0 || end > self.buf.len() {
                    if flush || len == 0 {
                        i += 1;
                        continue;
                    }
                    break None;
                }

                // The frame must be followed by another sync word (or the end of the stream).
                if end + LOAS_HEADER_LEN <= self.buf.len() {
                    if Self::loas_len(&self.buf[end..]).is_none() {
                        i += 1;
                        continue;
                    }
                }
                else if !flush {
                    break None;
                }

                break Some((i, end));
            };

            match found {
                Some((i, end)) => {
                    let start = self.base + i as u64;
                    let frame = self.buf[i + LOAS_HEADER_LEN..end].to_vec();
                    self.buf.drain(..end);
                    self.base += end as u64;

                    let payloads = match read_audio_mux_element(&frame, &mut self.config) {
                        Ok(payloads) => payloads,
                        Err(_) => continue,
                    };

                    if self.config.is_none() {
                        // Wait for the stream mux config.
                        if self.early.len() < LATM_MAX_EARLY_FRAMES {
                            self.early.push((start, frame));
                        }
                        continue;
                    }

                    // The frames before the config, that are complete, use the config. The
                    // durations are set when they are returned.
                    for (early_start, early) in std::mem::take(&mut self.early) {
                        if let Ok(payloads) = read_audio_mux_element(&early, &mut self.config) {
                            for data in payloads {
                                self.pending.push_back(EsFrame {
                                    start: early_start,
                                    data,
                                    dur: 0,
                                    trim_start: 0,
                                    trim_end: 0,
                                });
                            }
                        }
                    }

                    for data in payloads {
                        self.pending.push_back(EsFrame {
                            start,
                            data,
                            dur: 0,
                            trim_start: 0,
                            trim_end: 0,
                        });
                    }
                }
                None => {
                    self.buf.drain(..i);
                    self.base += i as u64;

                    // The end of the stream: the stream info is determined from the frames there
                    // are.
                    if flush && self.info.is_none() && self.finish_info(true) {
                        continue;
                    }

                    return None;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.pending.clear();
        self.early.clear();
        // The stream mux config is retained: a stream may only repeat it infrequently.
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        self.timeline.seek_target(target)
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        // The stream has been probed: the parser does not look for SBR again.
        let mut es = LatmEs::new();
        es.config = self.config.clone();
        es.info = self.info.clone();
        es.timeline = self.timeline;
        Box::new(es)
    }

    fn frame_grid(&self) -> u64 {
        self.timeline.frame_dur()
    }
}

/// The channel mapping tables of RFC 7845 §5.1.1.2, for streams with 3 to 8 channels, as the
/// number of streams, the number of coupled streams, and the channel mapping.
const OPUS_VORBIS_MAPPINGS: [(u8, u8, &[u8]); 6] = [
    (2, 1, &[0, 2, 1]),
    (2, 2, &[0, 1, 2, 3]),
    (3, 2, &[0, 4, 1, 2, 3]),
    (4, 2, &[0, 4, 1, 2, 3, 5]),
    (4, 3, &[0, 4, 1, 2, 3, 5, 6]),
    (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]),
];

/// Build an `OpusHead` identification header (RFC 7845 §5.1) for a stream of `channels` channels
/// with the Vorbis channel order. Returns `None` if the number of channels is not 1 to 8.
pub fn build_opus_head(channels: u8, pre_skip: u16) -> Option<Box<[u8]>> {
    let mut head = Vec::with_capacity(29);

    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());

    match channels {
        1 | 2 => head.push(0),
        3..=8 => {
            let (streams, coupled, map) = OPUS_VORBIS_MAPPINGS[usize::from(channels) - 3];
            head.push(1);
            head.push(streams);
            head.push(coupled);
            head.extend_from_slice(map);
        }
        _ => return None,
    }

    Some(head.into_boxed_slice())
}

/// The channels of an Opus stream with `channels` channels in the Vorbis order of RFC 7845.
pub fn opus_channels(channels: u8) -> Option<Channels> {
    use Position as P;

    Some(Channels::Positioned(match channels {
        1 => P::FRONT_LEFT,
        2 => P::FRONT_LEFT | P::FRONT_RIGHT,
        3 => P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT,
        4 => P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
        5 => P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
        6 => {
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::LFE1
        }
        7 => {
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
                | P::REAR_CENTER
                | P::LFE1
        }
        8 => {
            P::FRONT_LEFT
                | P::FRONT_CENTER
                | P::FRONT_RIGHT
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::LFE1
        }
        _ => return None,
    }))
}

/// Get the duration of an Opus packet in samples at 48 kHz from its table of contents (RFC 6716
/// §3.1). Returns `None` if the packet is malformed.
pub fn opus_packet_samples(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;

    // The duration of a frame in units of 0.5 ms * 4 (i.e., 1/8 ms), to represent 2.5 ms.
    let frame_dur_48k: u64 = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };

    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(packet.get(1)? & 0x3f),
    };

    if frames == 0 {
        return None;
    }

    let total = frame_dur_48k * frames;

    // A packet may not exceed 120 ms.
    if total > 5760 { None } else { Some(total) }
}

/// An Opus in MPEG-TS elementary stream parser (the "Opus access unit" framing with control
/// headers of ETSI TS 102 366 Annex of the "Opus in MPEG-2 Transport Stream" specification).
pub struct OpusEs {
    buf: Vec<u8>,
    base: u64,
    info: EsInfo,
}

impl OpusEs {
    /// Create a parser for an Opus stream with `channels` channels. Returns `None` if the
    /// number of channels is not supported.
    pub fn new(channels: u8) -> Option<Self> {
        let head = build_opus_head(channels, 0)?;

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_OPUS)
            .with_sample_rate(48_000)
            .with_channels(opus_channels(channels)?)
            .with_extra_data(head);

        Some(OpusEs { buf: Vec::new(), base: 0, info: EsInfo { params, rate: 48_000 } })
    }

    /// Parse the access unit control header at the start of `b`. Returns the length of the
    /// header, the length of the access unit, and the start and end trim.
    fn parse_header(b: &[u8]) -> Option<(usize, usize, u16, u16)> {
        let (&b0, &b1) = (b.first()?, b.get(1)?);

        if b0 != 0x7f || b1 & 0xe0 != 0xe0 {
            return None;
        }

        let start_trim = b1 & 0x10 != 0;
        let end_trim = b1 & 0x08 != 0;
        let extension = b1 & 0x04 != 0;

        let mut i = 2;
        let mut au_len = 0usize;

        loop {
            let v = *b.get(i)?;
            i += 1;
            au_len += usize::from(v);

            if v != 0xff {
                break;
            }
        }

        let mut trims = [0u16; 2];

        for (idx, flag) in [start_trim, end_trim].into_iter().enumerate() {
            if flag {
                let v = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
                i += 2;
                trims[idx] = v & 0x1fff;
            }
        }

        if extension {
            let len = usize::from(*b.get(i)?);
            i += 1 + len;
        }

        Some((i, au_len, trims[0], trims[1]))
    }
}

impl EsParser for OpusEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        let mut i = 0;

        loop {
            while i + 1 < self.buf.len() && !(self.buf[i] == 0x7f && self.buf[i + 1] & 0xe0 == 0xe0)
            {
                i += 1;
            }

            let Some((hdr_len, au_len, start_trim, end_trim)) = Self::parse_header(&self.buf[i..])
            else {
                // Not enough data for a header, or not a header.
                if self.buf.len() >= i + 2
                    && !(self.buf[i] == 0x7f && self.buf[i + 1] & 0xe0 == 0xe0)
                {
                    i = self.buf.len();
                }
                break;
            };

            let end = i + hdr_len + au_len;

            if end > self.buf.len() {
                if flush {
                    i += 1;
                    continue;
                }
                break;
            }

            let packet = self.buf[i + hdr_len..end].to_vec();
            let start = self.base + i as u64;

            self.buf.drain(..end);
            self.base += end as u64;

            // Skip malformed access units.
            let Some(total) = opus_packet_samples(&packet)
            else {
                i = 0;
                continue;
            };

            let trim_start = u64::from(start_trim).min(total);
            let trim_end = u64::from(end_trim).min(total - trim_start);

            return Some(EsFrame {
                start,
                data: packet.into_boxed_slice(),
                dur: total - trim_start - trim_end,
                trim_start,
                trim_end,
            });
        }

        self.buf.drain(..i);
        self.base += i as u64;
        None
    }

    fn clear(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        Some(&self.info)
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        target - OPUS_SEEK_PREROLL
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        let channels = self.info.params.channels.as_ref().map_or(2, |c| c.count() as u8);
        Box::new(OpusEs::new(channels).expect("channel count was validated"))
    }
}

/// A packet with a timestamp assigned.
struct Pending {
    frame: EsFrame,
    pts: Option<u64>,
}

/// An `EsTrack` is the audio elementary stream of one track: it is given the payload of the PES
/// packets of the stream, and produces timestamped packets.
pub struct EsTrack {
    /// The track ID of the packets.
    pub track_id: u32,
    parser: Box<dyn EsParser>,
    timeline: Timeline,
    /// The offset into the elementary stream, and raw PTS, of the PES packets pushed.
    anchors: VecDeque<(u64, u64)>,
    /// The number of bytes pushed.
    pushed: u64,
    /// Parsed frames that have not been timestamped, so that the stream info is available early.
    pending: VecDeque<Pending>,
    /// The number of ticks trimmed from the start of the first frame of the stream.
    lead_in: Option<u64>,
}

impl EsTrack {
    /// Create a track for the elementary stream `parser`.
    pub fn new(track_id: u32, parser: Box<dyn EsParser>) -> Self {
        EsTrack {
            track_id,
            parser,
            timeline: Timeline::new(1),
            anchors: VecDeque::new(),
            pushed: 0,
            pending: VecDeque::new(),
            lead_in: None,
        }
    }

    /// The number of ticks that are trimmed from the start of the first frame of the stream
    /// (encoder delay), once the first frame has been parsed.
    pub fn lead_in(&self) -> Option<u64> {
        self.lead_in
    }

    /// The stream information, once available.
    pub fn info(&self) -> Option<&EsInfo> {
        self.parser.info()
    }

    /// The parser.
    pub fn parser(&self) -> &dyn EsParser {
        self.parser.as_ref()
    }

    /// The timeline.
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// The timeline.
    pub fn timeline_mut(&mut self) -> &mut Timeline {
        &mut self.timeline
    }

    /// Push the payload of a PES packet with the raw (33-bit) PTS `pts`, if it has one.
    pub fn push_pes(&mut self, pts: Option<u64>, payload: &[u8]) {
        if let Some(pts) = pts {
            self.anchors.push_back((self.pushed, pts));
        }

        self.parser.push(payload);
        self.pushed += payload.len() as u64;
    }

    /// Parse frames from the data pushed so far into the pending queue.
    fn parse(&mut self, flush: bool) {
        while let Some(frame) = self.parser.next_frame(flush) {
            // The anchor of the frame is the last PES packet that begins at or before the frame.
            let mut pts = None;

            while let Some(&(offset, anchor)) = self.anchors.front() {
                if offset > frame.start {
                    break;
                }
                pts = Some(anchor);
                self.anchors.pop_front();
            }

            if self.lead_in.is_none() {
                self.lead_in = Some(frame.trim_start);
            }

            self.pending.push_back(Pending { frame, pts });
        }
    }

    /// Parse the data pushed so far to determine the stream info. Returns true if it is known.
    pub fn probe(&mut self, flush: bool) -> bool {
        if self.pending.is_empty() {
            self.parse(flush);
        }

        self.info().is_some()
    }

    /// Set the sample rate of the timeline. Must be called after the stream info is known and
    /// before packets are popped.
    pub fn init_timeline(&mut self, base: Option<u64>) {
        let rate = self.info().map_or(1, |i| i.rate);
        self.timeline = Timeline::new(rate);
        self.timeline.set_grid(self.parser.frame_grid());

        if let Some(base) = base {
            self.timeline.set_base(base);
        }
    }

    /// Pop the next packet. If `flush` is true, no more data will be pushed. Frames without a
    /// preceding PES packet timestamp continue from the previous frame.
    pub fn pop(&mut self, flush: bool) -> Option<Packet> {
        if self.pending.is_empty() {
            self.parse(flush);
        }

        let Pending { frame, pts } = self.pending.pop_front()?;

        // The timeline counts the whole decoded frame, including trimmed samples.
        let block = frame.dur + frame.trim_start + frame.trim_end;
        let ts = self.timeline.stamp(pts, block);

        let mut packet = Packet::new(
            self.track_id,
            Timestamp::new(ts + frame.trim_start as i64),
            Duration::new(frame.dur),
            frame.data,
        );

        packet.trim_start = Duration::new(frame.trim_start);
        packet.trim_end = Duration::new(frame.trim_end);
        packet.dts = Timestamp::new(ts);

        Some(packet)
    }

    /// Discard all buffered data after a seek, to continue at the PES packet with the PTS
    /// `hint_pts`.
    pub fn seek(&mut self, hint_clock: i64) {
        self.parser.clear();
        self.anchors.clear();
        self.pending.clear();
        self.timeline.discontinuity(hint_clock);
    }

    /// Returns the duration, in ticks, from the start of the first frame in the PES `payloads` to
    /// the end of the samples of the last that are not trimmed from the end, as parsed by a fresh
    /// parser.
    pub fn payload_duration(&self, payloads: &[Vec<u8>]) -> u64 {
        let mut parser = self.parser.fresh();
        let mut dur = 0;

        for payload in payloads {
            parser.push(payload);

            while let Some(frame) = parser.next_frame(false) {
                dur += frame.dur + frame.trim_start;
            }
        }

        while let Some(frame) = parser.next_frame(true) {
            dur += frame.dur + frame.trim_start;
        }

        dur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts_frame(payload_len: usize, fill: u8) -> Vec<u8> {
        // MPEG-4 LC, 44.1 kHz (index 4), stereo, no CRC.
        let frame_len = payload_len + 7;
        let mut f = vec![
            0xff,
            0xf1,
            (1 << 6) | (4 << 2),
            (2 << 6) | ((frame_len >> 11) & 3) as u8,
            ((frame_len >> 3) & 0xff) as u8,
            (((frame_len & 7) << 5) | 0x1f) as u8,
            0xfc,
        ];
        f.extend(std::iter::repeat_n(fill, payload_len));
        f
    }

    #[test]
    fn verify_adts_framing_across_pushes() {
        let mut stream = vec![];
        for i in 0..6u8 {
            stream.extend(adts_frame(50 + usize::from(i) * 7, i));
        }

        for chunk in [1usize, 5, 64, 1000] {
            let mut es = AdtsEs::new();
            let mut got = vec![];

            for c in stream.chunks(chunk) {
                es.push(c);
                while let Some(f) = es.next_frame(false) {
                    got.push((f.data.len(), f.data[0]));
                }
            }
            while let Some(f) = es.next_frame(true) {
                got.push((f.data.len(), f.data[0]));
            }

            let want: Vec<_> = (0..6u8).map(|i| (50 + usize::from(i) * 7, i)).collect();
            assert_eq!(got, want, "chunk={chunk}");

            let info = es.info().unwrap();
            assert_eq!(info.rate, 44_100);
            assert_eq!(info.params.channels.as_ref().unwrap().count(), 2);
        }
    }

    fn opus_au(toc: u8, len: usize, start_trim: Option<u16>) -> Vec<u8> {
        let mut au = vec![0x7f, 0xe0 | if start_trim.is_some() { 0x10 } else { 0 }];
        let mut rem = len;
        while rem >= 255 {
            au.push(255);
            rem -= 255;
        }
        au.push(rem as u8);
        if let Some(t) = start_trim {
            au.extend_from_slice(&t.to_be_bytes());
        }
        au.push(toc);
        au.extend(std::iter::repeat_n(0u8, len - 1));
        au
    }

    #[test]
    fn verify_opus_access_units() {
        let mut es = OpusEs::new(2).unwrap();
        // CELT FB 20 ms (config 31), mono, one frame: 960 samples.
        let mut pes = opus_au(31 << 3, 300, Some(312));
        pes.extend(opus_au(31 << 3, 10, None));
        es.push(&pes);

        let a = es.next_frame(false).unwrap();
        assert_eq!((a.dur, a.trim_start, a.start), (960 - 312, 312, 0));
        assert_eq!(a.data.len(), 300);

        let b = es.next_frame(false).unwrap();
        assert_eq!((b.dur, b.trim_start), (960, 0));
        assert!(es.next_frame(true).is_none());
    }

    #[test]
    fn verify_opus_head() {
        let head = build_opus_head(6, 0).unwrap();
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], 6);
        assert_eq!(head[18], 1);
        assert_eq!(&head[19..21], &[4, 2]);
        assert_eq!(head.len(), 21 + 6);
        assert_eq!(build_opus_head(2, 0).unwrap().len(), 19);
        assert!(build_opus_head(9, 0).is_none());
    }

    /// A bit writer, for building LATM.
    #[derive(Default)]
    struct Bits {
        bytes: Vec<u8>,
        n: usize,
    }

    impl Bits {
        fn put(&mut self, value: u32, bits: usize) {
            for i in (0..bits).rev() {
                if self.n % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.n % 8);
                self.n += 1;
            }
        }
    }

    /// A LOAS frame with an `AudioMuxElement` of a single AAC-LC, 44.1 kHz, stereo, payload.
    fn loas_frame(with_config: bool, payload: &[u8]) -> Vec<u8> {
        let mut b = Bits::default();

        // useSameStreamMux
        b.put(u32::from(!with_config), 1);

        if with_config {
            b.put(0, 1); // audioMuxVersion
            b.put(1, 1); // allStreamsSameTimeFraming
            b.put(0, 6); // numSubFrames
            b.put(0, 4); // numProgram
            b.put(0, 3); // numLayer
            b.put(0x1210, 16); // The audio specific config.
            b.put(0, 3); // frameLengthType
            b.put(0xff, 8); // latmBufferFullness
            b.put(0, 1); // otherDataPresent
            b.put(0, 1); // crcCheckPresent
        }

        // PayloadLengthInfo, and PayloadMux.
        b.put(payload.len() as u32, 8);

        for &byte in payload {
            b.put(u32::from(byte), 8);
        }

        let len = b.bytes.len();
        let mut f = vec![0x56, 0xe0 | (len >> 8) as u8, len as u8];
        f.extend_from_slice(&b.bytes);
        f
    }

    #[test]
    fn verify_latm_with_frames_before_the_stream_mux_config() {
        let mut stream = vec![];

        // Two frames before the config is received, then the config repeats.
        for i in 0..6u8 {
            stream.extend(loas_frame(i == 2 || i == 5, &[i; 20]));
        }

        for chunk in [1usize, 7, 1000] {
            let mut es = LatmEs::new();
            let mut got = vec![];

            for c in stream.chunks(chunk) {
                es.push(c);
                while let Some(f) = es.next_frame(false) {
                    got.push((f.data[0], f.data.len(), f.dur));
                }
            }

            while let Some(f) = es.next_frame(true) {
                got.push((f.data[0], f.data.len(), f.dur));
            }

            let want: Vec<_> = (0..6u8).map(|i| (i, 20, 1024)).collect();
            assert_eq!(got, want, "chunk={chunk}");

            let info = es.info().unwrap();
            assert_eq!(info.rate, 44_100);
            assert_eq!(info.params.channels.as_ref().unwrap().count(), 2);
            assert_eq!(info.params.extra_data.as_deref(), Some(&[0x12, 0x10][..]));
        }
    }

    #[test]
    fn verify_track_timestamps() {
        let mut track = EsTrack::new(7, Box::new(MpaEs::new()));

        // MPEG-1 layer 2, 128 kbps, 48 kHz: 384 byte frames of 1152 samples.
        let word: u32 =
            0xffe0_0000 | (0b11 << 19) | (0b10 << 17) | (1 << 16) | (8 << 12) | (1 << 10);
        let info = mpa::parse_header(word).unwrap();
        assert_eq!(info.frame_len, 384);
        let mut frame = vec![0u8; 384];
        frame[..4].copy_from_slice(&word.to_be_bytes());

        let pes: Vec<u8> = frame.iter().cloned().cycle().take(384 * 4).collect();
        track.push_pes(Some(90_000), &pes);
        track.push_pes(Some(90_000 + 4 * 1152 * 90_000 / 48_000), &pes);

        assert!(track.probe(true));
        track.init_timeline(Some(90_000));

        let mut pts = vec![];
        while let Some(p) = track.pop(true) {
            pts.push((p.pts.get(), p.dur.get()));
        }

        assert_eq!(pts.len(), 8);
        assert_eq!(pts[0], (0, 1152));
        assert_eq!(pts[7], (7 * 1152, 1152));
    }
}
