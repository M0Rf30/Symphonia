// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MPEG audio frame splitting.

static BIT_RATES: [[u32; 15]; 5] = [
    // MPEG 1 layer I, II, III.
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    // MPEG 2 and 2.5 layer I, and II/III.
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// A parsed MPEG audio frame header.
pub struct FrameHeader {
    /// The length of the frame in bytes.
    pub len: usize,
    /// The number of samples per channel of the frame.
    pub samples: u64,
}

/// Parses the header word of an MPEG audio frame.
pub fn parse_header(word: u32) -> Option<FrameHeader> {
    if word & 0xffe0_0000 != 0xffe0_0000 {
        return None;
    }

    let version = (word >> 19) & 3;
    let layer = (word >> 17) & 3;
    let bitrate_idx = ((word >> 12) & 0xf) as usize;
    let sr_idx = ((word >> 10) & 3) as usize;
    let padding = (word >> 9) & 1;

    if version == 1 || layer == 0 || bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return None;
    }

    let mpeg1 = version == 3;

    let table = match (mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, _) => 2,
        (false, 3) => 3,
        (false, _) => 4,
    };

    let bitrate = BIT_RATES[table][bitrate_idx] * 1000;

    let base = [44_100, 48_000, 32_000][sr_idx];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };

    let (samples, factor, slot) = match layer {
        3 => (384, 12, 4),
        2 => (1152, 144, 1),
        _ if mpeg1 => (1152, 144, 1),
        _ => (576, 72, 1),
    };

    let len = ((factor * bitrate / rate + padding) * slot) as usize;

    if len <= 4 {
        return None;
    }

    Some(FrameHeader { len, samples })
}

/// Splits a chunk into whole frames, skipping bytes that are not part of a frame. Returns the
/// ranges of the frames and their number of samples per channel.
pub fn split_frames(data: &[u8]) -> Vec<(usize, usize, u64)> {
    let mut frames = Vec::new();
    let mut pos = 0;

    while pos + 4 <= data.len() {
        let word = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);

        match parse_header(word) {
            Some(hdr) if pos + hdr.len <= data.len() => {
                frames.push((pos, pos + hdr.len, hdr.samples));
                pos += hdr.len;
            }
            _ => pos += 1,
        }
    }

    frames
}
