// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Elementary stream parsers for audio carried in FLV audio tags, where each tag is a frame (or
//! a number of frames) of the codec.

use std::collections::VecDeque;

use symphonia_codec_aac::detect_implicit_sbr;
use symphonia_common::mpeg::audio::{
    AudioSpecificConfig, MAX_IMPLICIT_SBR_PROBE_BLOCKS, may_have_implicit_sbr,
};
use symphonia_common::mpeg::es::{AacTimeline, EsFrame, EsInfo, EsParser, aac_es_info};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::errors::{Error, Result, decode_error};

/// AAC raw data blocks, with the codec configuration from the audio specific config.
///
/// Like the AAC readers, the stream info describes the decoded output and the timeline is in
/// decoded frames. A stream of AAC-LC whose config cannot signal SBR may be HE-AAC: its first
/// blocks are looked at before the stream info is known (see [`detect_implicit_sbr`]).
pub struct RawAacEs {
    asc_bytes: Box<[u8]>,
    info: Option<EsInfo>,
    timeline: AacTimeline,
    /// True if the stream info waits for the first blocks to look for implicit SBR.
    detecting: bool,
    pending: VecDeque<EsFrame>,
    pushed: u64,
}

impl RawAacEs {
    /// Create a parser from the audio specific config of the stream.
    pub fn new(asc_bytes: &[u8]) -> Result<Self> {
        if asc_bytes.len() < 2 {
            return decode_error("flv: invalid aac audio specific config");
        }

        let asc = AudioSpecificConfig::read(asc_bytes)?;

        if asc.sample_rate == 0 {
            return Err(Error::DecodeError("flv: invalid aac sample rate"));
        }

        let detecting = may_have_implicit_sbr(&asc);

        Ok(RawAacEs {
            asc_bytes: asc_bytes.into(),
            info: (!detecting).then(|| aac_es_info(&asc, asc_bytes)),
            timeline: AacTimeline::from_config(&asc),
            detecting,
            pending: VecDeque::new(),
            pushed: 0,
        })
    }

    /// Determine the stream info from the first blocks. Returns false if more are required.
    fn finish_detection(&mut self, flush: bool) -> bool {
        if self.pending.len() < MAX_IMPLICIT_SBR_PROBE_BLOCKS && !flush {
            return false;
        }

        let blocks: Vec<&[u8]> = self
            .pending
            .iter()
            .take(MAX_IMPLICIT_SBR_PROBE_BLOCKS)
            .map(|frame| &frame.data[..])
            .collect();

        let explicit = detect_implicit_sbr(&self.asc_bytes, &blocks);

        let (asc, bytes) = match explicit {
            Some(bytes) => (AudioSpecificConfig::read(&bytes).ok(), Some(bytes)),
            None => (None, None),
        };

        let (asc, bytes) = match (asc, bytes) {
            (Some(asc), Some(bytes)) => (asc, bytes),
            // The config is known to be valid.
            _ => match AudioSpecificConfig::read(&self.asc_bytes) {
                Ok(asc) => (asc, self.asc_bytes.clone()),
                Err(_) => return false,
            },
        };

        self.timeline = AacTimeline::from_config(&asc);
        self.info = Some(aac_es_info(&asc, &bytes));
        self.detecting = false;
        true
    }
}

impl EsParser for RawAacEs {
    fn push(&mut self, data: &[u8]) {
        let start = self.pushed;
        self.pushed += data.len() as u64;

        if !data.is_empty() {
            self.pending.push_back(EsFrame {
                start,
                data: data.into(),
                dur: 0,
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        if self.detecting && !self.finish_detection(flush) {
            return None;
        }

        let mut frame = self.pending.pop_front()?;
        frame.dur = self.timeline.frame_dur();
        Some(frame)
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        self.timeline.seek_target(target)
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(RawAacEs {
            asc_bytes: self.asc_bytes.clone(),
            info: self.info.clone(),
            timeline: self.timeline,
            detecting: self.detecting,
            pending: VecDeque::new(),
            pushed: 0,
        })
    }

    fn frame_grid(&self) -> u64 {
        self.timeline.frame_dur()
    }
}

/// Raw PCM (including A-law and mu-law) audio: each push is a number of whole sample frames.
pub struct PcmEs {
    info: EsInfo,
    /// The number of bytes of one sample of all channels.
    frame_bytes: usize,
    pending: VecDeque<EsFrame>,
    pushed: u64,
}

impl PcmEs {
    /// Create a parser from the codec parameters, and the number of bytes of a sample frame.
    pub fn new(params: AudioCodecParameters, frame_bytes: usize) -> Self {
        let rate = params.sample_rate.unwrap_or(1);

        PcmEs {
            info: EsInfo { params, rate },
            frame_bytes: frame_bytes.max(1),
            pending: VecDeque::new(),
            pushed: 0,
        }
    }
}

impl EsParser for PcmEs {
    fn push(&mut self, data: &[u8]) {
        let start = self.pushed;
        self.pushed += data.len() as u64;

        let frames = data.len() / self.frame_bytes;

        if frames > 0 {
            self.pending.push_back(EsFrame {
                start,
                data: data[..frames * self.frame_bytes].into(),
                dur: frames as u64,
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn next_frame(&mut self, _flush: bool) -> Option<EsFrame> {
        self.pending.pop_front()
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        Some(&self.info)
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        target
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(PcmEs::new(self.info.params.clone(), self.frame_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::codecs::audio::well_known::CODEC_ID_PCM_S16LE;

    #[test]
    fn verify_raw_aac() {
        // AAC-LC, 44.1 kHz, stereo.
        let mut es = RawAacEs::new(&[0x12, 0x10]).unwrap();
        es.push(&[1, 2, 3]);
        es.push(&[]);
        es.push(&[4]);

        let a = es.next_frame(false).unwrap();
        assert_eq!((a.dur, a.start, &a.data[..]), (1024, 0, &[1u8, 2, 3][..]));
        let b = es.next_frame(false).unwrap();
        assert_eq!((b.start, &b.data[..]), (3, &[4u8][..]));
        assert!(es.next_frame(true).is_none());

        let info = es.info().unwrap();
        assert_eq!(info.rate, 44_100);
        assert_eq!(info.params.sample_rate, Some(44_100));
        assert_eq!(info.params.channels.as_ref().unwrap().count(), 2);

        assert!(RawAacEs::new(&[0x12]).is_err());
    }

    #[test]
    fn verify_pcm() {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_PCM_S16LE).with_sample_rate(22_050);

        let mut es = PcmEs::new(params, 4);
        es.push(&[0u8; 10]);

        let f = es.next_frame(false).unwrap();
        assert_eq!((f.dur, f.data.len()), (2, 8));
        assert_eq!(es.info().unwrap().rate, 22_050);
    }
}
