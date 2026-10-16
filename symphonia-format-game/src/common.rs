// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the readers.

use std::io::ErrorKind;
use std::num::NonZero;

use symphonia_core::audio::{Channels, layouts};
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error};
use symphonia_core::formats::SeekTo;
use symphonia_core::io::ReadBytes;
use symphonia_core::units::{TimeBase, Timestamp};

/// Fills `buf` from `src`, stopping early only at the end of the stream. Returns the number of
/// bytes read.
pub(crate) fn read_up_to<B: ReadBytes>(src: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut len = 0;

    while len < buf.len() {
        match src.read_buf(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(len)
}

/// The time base of a stream whose timestamps are in frames at `rate` Hz.
pub(crate) fn frame_time_base(rate: u32) -> Option<TimeBase> {
    NonZero::new(rate).map(|rate| TimeBase::new(NonZero::<u32>::MIN, rate))
}

/// The channels of a stream of `count` channels. FFmpeg leaves the layout of these streams
/// unspecified, so only the unambiguous layouts are positioned.
pub(crate) fn channels_for(count: usize) -> Channels {
    match count {
        1 => layouts::CHANNEL_LAYOUT_MONO,
        2 => layouts::CHANNEL_LAYOUT_STEREO,
        n => Channels::Discrete(n as u16),
    }
}

/// Resolves a seek request to a timestamp in the units of `time_base`. Negative timestamps are out
/// of range.
pub(crate) fn required_timestamp(to: SeekTo, time_base: TimeBase) -> Result<u64> {
    let ts = match to {
        SeekTo::Timestamp { ts, .. } => ts,
        SeekTo::Time { time, .. } => time_base.calc_timestamp(time).unwrap_or(Timestamp::MAX),
    };

    if ts.is_negative() {
        return seek_error(SeekErrorKind::OutOfRange);
    }

    Ok(ts.get() as u64)
}

/// Returns an `Error` for a stream that cannot be seeked.
pub(crate) fn unseekable<T>() -> Result<T> {
    Err(Error::SeekError(SeekErrorKind::Unseekable))
}
