// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::Channels;
use symphonia_core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{MediaSource, MediaSourceStream};

/// Maps a channel count to a set of channels. The files of these formats do not state a layout:
/// one channel is mono, two are left and right, and any more are discrete channels.
pub(crate) fn channels_from_count(count: u32) -> Result<Channels> {
    match count {
        0 => decode_error("sound: invalid channel count"),
        1 => Ok(CHANNEL_LAYOUT_MONO),
        2 => Ok(CHANNEL_LAYOUT_STEREO),
        n => match u16::try_from(n) {
            Ok(n) => Ok(Channels::Discrete(n)),
            Err(_) => decode_error("sound: too many channels"),
        },
    }
}

/// Limits the end of a run of data that begins at `start` and is `len` bytes long, to the end of
/// the stream if it is known. Returns the position after the last byte of the data.
pub(crate) fn clamp_data_end(
    mss: &MediaSourceStream<'_>,
    start: u64,
    len: Option<u64>,
) -> Option<u64> {
    let end = len.and_then(|len| start.checked_add(len));
    let stream_end = mss.byte_len();

    match (end, stream_end) {
        (Some(end), Some(stream_end)) => Some(end.min(stream_end)),
        (None, Some(stream_end)) => Some(stream_end),
        (end, None) => end,
    }
}
