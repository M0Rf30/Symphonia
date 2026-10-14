// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Identification of the codec of an elementary stream from its data.

use symphonia_common::mpeg::es::{EsFrame, EsInfo, EsParser};

/// An elementary stream parser for a stream of unidentified audio: PES private data (stream type
/// 0x06) without descriptors, as written by some multiplexers for BDAV streams. The data is given
/// to a number of candidate parsers, and the stream is that of the first to find a frame.
pub struct SniffEs {
    candidates: Vec<Box<dyn EsParser>>,
    chosen: Option<Box<dyn EsParser>>,
}

impl SniffEs {
    /// Create a parser that identifies the stream as one of `candidates`, which are tried in the
    /// order given.
    pub fn new(candidates: Vec<Box<dyn EsParser>>) -> Self {
        SniffEs { candidates, chosen: None }
    }
}

impl EsParser for SniffEs {
    fn push(&mut self, data: &[u8]) {
        match self.chosen.as_mut() {
            Some(parser) => parser.push(data),
            None => self.candidates.iter_mut().for_each(|c| c.push(data)),
        }
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        if let Some(parser) = self.chosen.as_mut() {
            return parser.next_frame(flush);
        }

        let (idx, frame) = self
            .candidates
            .iter_mut()
            .enumerate()
            .find_map(|(i, c)| c.next_frame(flush).map(|f| (i, f)))?;

        self.chosen = Some(self.candidates.swap_remove(idx));
        self.candidates.clear();

        Some(frame)
    }

    fn clear(&mut self) {
        match self.chosen.as_mut() {
            Some(parser) => parser.clear(),
            None => self.candidates.iter_mut().for_each(|c| c.clear()),
        }
    }

    fn info(&self) -> Option<&EsInfo> {
        self.chosen.as_ref()?.info()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        self.chosen.as_ref().map_or(target, |p| p.seek_start_ts(target))
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        match self.chosen.as_ref() {
            Some(parser) => parser.fresh(),
            None => Box::new(SniffEs::new(self.candidates.iter().map(|c| c.fresh()).collect())),
        }
    }

    fn frame_grid(&self) -> u64 {
        self.chosen.as_ref().map_or(0, |p| p.frame_grid())
    }
}
