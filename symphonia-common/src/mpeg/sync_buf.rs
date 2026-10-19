// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The buffer of the elementary stream parsers.

use std::ops::Deref;

/// A byte buffer that tracks the absolute stream offset of its first unconsumed byte.
///
/// Consuming bytes only advances a read offset. The consumed bytes are dropped when more data is
/// pushed, so that a parser that returns many frames from one chunk moves the remaining data once
/// per chunk, not once per frame.
#[derive(Default)]
pub(crate) struct SyncBuffer {
    buf: Vec<u8>,
    /// The number of consumed bytes at the start of `buf`.
    pos: usize,
    /// The absolute offset of the first unconsumed byte.
    base: u64,
    /// The number of unconsumed bytes at the start that are known not to start a marker.
    scanned: usize,
}

impl SyncBuffer {
    /// Append data.
    pub fn push(&mut self, data: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }

        self.buf.extend_from_slice(data);
    }

    /// The absolute offset of the first unconsumed byte.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The absolute offset one past the last byte pushed.
    pub fn end_offset(&self) -> u64 {
        self.base + self.len() as u64
    }

    /// Discard the first `n` unconsumed bytes.
    pub fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len());

        let n = n.min(self.len());

        self.pos += n;
        self.scanned = self.scanned.saturating_sub(n);
        self.base += n as u64;

        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
    }

    /// Find the first offset at or after the search cursor where `pred` accepts the unconsumed
    /// data from that offset on, which is at least `marker_len` bytes long.
    ///
    /// If no marker is found, all data that cannot be the start of a marker is discarded, keeping
    /// the last `marker_len - 1` bytes that may hold a partial marker, and `None` is returned. The
    /// search cursor lets a repeated search after `consume` skip the data already searched.
    pub fn seek_marker(
        &mut self,
        pred: impl Fn(&[u8]) -> bool,
        marker_len: usize,
    ) -> Option<usize> {
        let data = &self.buf[self.pos..];
        let last = (data.len() + 1).saturating_sub(marker_len);

        match (self.scanned..last).find(|&i| pred(&data[i..])) {
            Some(i) => {
                self.scanned = i;
                Some(i)
            }
            None => {
                let drop = last.min(data.len());
                self.consume(drop);
                self.scanned = 0;
                None
            }
        }
    }

    /// Discard all data. The absolute offset continues.
    pub fn clear(&mut self) {
        self.consume(self.len());
    }
}

impl Deref for SyncBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_consume_and_offsets() {
        let mut buf = SyncBuffer::default();

        buf.push(&[1, 2, 3, 4, 5]);
        buf.consume(2);
        assert_eq!(&buf[..], &[3, 4, 5]);
        assert_eq!(buf.base(), 2);
        assert_eq!(buf.end_offset(), 5);

        buf.push(&[6, 7]);
        assert_eq!(&buf[..], &[3, 4, 5, 6, 7]);
        assert_eq!(buf.base(), 2);

        buf.consume(3);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.base(), 7);
        assert_eq!(buf.end_offset(), 7);
    }

    #[test]
    fn verify_seek_marker_bounds_buffer() {
        let mut buf = SyncBuffer::default();
        let zeros = [0u8; 4096];

        for _ in 0..(4 * 256) {
            buf.push(&zeros);
            assert_eq!(buf.seek_marker(|w| w.starts_with(&[0x0b, 0x77]), 2), None);
            assert!(buf.len() <= 1);
        }

        // A marker split across pushes is kept.
        buf.push(&[0x0b]);
        assert_eq!(buf.seek_marker(|w| w.starts_with(&[0x0b, 0x77]), 2), None);
        buf.push(&[0x77, 9]);
        assert_eq!(buf.seek_marker(|w| w.starts_with(&[0x0b, 0x77]), 2), Some(0));
        buf.consume(1);
        assert_eq!(buf.seek_marker(|w| w.starts_with(&[0x0b, 0x77]), 2), None);
    }
}
