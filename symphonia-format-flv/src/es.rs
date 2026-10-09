// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Elementary stream parsers for audio carried in FLV audio tags, where each tag is a frame (or
//! a number of frames) of the codec.

use std::collections::VecDeque;

use symphonia_common::mpeg::audio::{AudioSpecificConfig, get_audio_codec_profile};
use symphonia_common::mpeg::es::{EsFrame, EsInfo, EsParser, aac_seek_target};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::errors::{Error, Result, decode_error};

/// AAC raw data blocks, with the codec configuration from the audio specific config.
pub struct RawAacEs {
    info: EsInfo,
    sbr: bool,
    frame_len: u64,
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

        let core_rate = Some(asc.sample_rate)
            .filter(|&r| r > 0)
            .ok_or(Error::DecodeError("flv: invalid aac sample rate"))?;

        let mut params = AudioCodecParameters::new();

        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(asc.output_sample_rate())
            .with_extra_data(asc_bytes.into());

        if let Some(channels) = asc.output_channels() {
            params.with_channels(channels);
        }

        if let Some(profile) = get_audio_codec_profile(&asc) {
            params.with_profile(profile);
        }

        Ok(RawAacEs {
            info: EsInfo { params, rate: core_rate },
            sbr: asc.sbr_present || asc.sample_rate <= 32_000,
            frame_len: asc.samples.max(1) as u64,
            pending: VecDeque::new(),
            pushed: 0,
        })
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
                dur: self.frame_len,
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
        aac_seek_target(target, self.sbr)
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(RawAacEs {
            info: self.info.clone(),
            sbr: self.sbr,
            frame_len: self.frame_len,
            pending: VecDeque::new(),
            pushed: 0,
        })
    }

    fn frame_grid(&self) -> u64 {
        self.frame_len
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
