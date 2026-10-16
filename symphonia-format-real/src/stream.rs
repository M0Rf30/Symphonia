// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An audio stream of a RealMedia file: the de-interleaving of the frames of the packets of the
//! container, and the time stamps of the frames.
//!
//! The container packets are assembled into the frames that the decoders take as packets, as
//! FFmpeg's `ff_rm_parse_packet` and `ff_rm_retrieve_cache` (`libavformat/rmdec.c`) do:
//!
//! * `genr` (Cook, ATRAC3): a group of `sub_packet_h` container packets carries `sub_packet_h *
//!   frame_size / sub_packet_size` frames that are spread over the group, so that the loss of a
//!   packet does not lose adjacent frames.
//! * `Int4` (RealAudio 2.0): the same, with the frames of `coded_frame_size` bytes.
//! * `sipr` (SIPR): as `genr` but the group is one frame per packet, and the 4 bit nibbles of the
//!   assembled group are swapped (`libavformat/rmsipr.c`).
//! * `vbrs` and `vbrf` (AAC): a packet holds up to 15 frames, with a table of their lengths.
//! * Otherwise a container packet is one or more frames.

use std::collections::VecDeque;

use log::{debug, warn};
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_common::mpeg::es::EsParser;
use symphonia_common::mpeg::surround::Ac3Es;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_AC3, CODEC_ID_ATRAC3, CODEC_ID_COOK, CODEC_ID_RA10, CODEC_ID_RA20,
    CODEC_ID_SIPR,
};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use crate::header::{AudioInfo, Interleave, RA144_FRAME_LEN};

/// The number of samples of an AAC frame.
const AAC_FRAME_LEN: u64 = 1024;

/// The container flag of a packet that is the start of a group of frames (a key frame).
pub(crate) const FLAG_KEY: u8 = 2;

/// The positions of the 4 bit blocks that are swapped by SIPR, in FFmpeg's `sipr_swaps`
/// (`libavformat/rmsipr.c`).
const SIPR_SWAPS: [[u8; 2]; 38] = [
    [0, 63],
    [1, 22],
    [2, 44],
    [3, 90],
    [5, 81],
    [7, 31],
    [8, 86],
    [9, 58],
    [10, 36],
    [12, 68],
    [13, 39],
    [14, 73],
    [15, 53],
    [16, 69],
    [17, 57],
    [19, 88],
    [20, 34],
    [21, 71],
    [24, 46],
    [25, 94],
    [26, 54],
    [28, 75],
    [29, 50],
    [32, 70],
    [33, 92],
    [35, 74],
    [38, 85],
    [40, 56],
    [42, 87],
    [43, 65],
    [45, 59],
    [48, 79],
    [49, 93],
    [51, 89],
    [55, 95],
    [61, 76],
    [67, 83],
    [77, 80],
];

/// Undo the bit reordering of a group of SIPR frames. FFmpeg's `ff_rm_reorder_sipr_data`.
fn reorder_sipr(buf: &mut [u8], sub_packet_h: usize, frame_size: usize) {
    // The number of nibbles in a block.
    let bs = sub_packet_h * frame_size * 2 / 96;

    // The highest nibble index is below `96 * bs`, which is at most the number of nibbles.
    if buf.len() * 2 < 96 * bs {
        return;
    }

    let get = |buf: &[u8], n: usize| (buf[n >> 1] >> (4 * (n & 1))) & 0xf;

    let set = |buf: &mut [u8], n: usize, v: u8| {
        buf[n >> 1] = (v << (4 * (n & 1))) | (buf[n >> 1] & (0xf << (4 * ((n & 1) ^ 1))));
    };

    for [a, b] in SIPR_SWAPS {
        let (a, b) = (bs * usize::from(a), bs * usize::from(b));

        // Swap the blocks of nibbles.
        for j in 0..bs {
            let (x, y) = (get(buf, a + j), get(buf, b + j));
            set(buf, b + j, x);
            set(buf, a + j, y);
        }
    }
}

/// The time of the frames of a stream.
///
/// The time stamps of the container are in milliseconds and are only given to the first packet of
/// a group of frames (and are absent in `.ra` files), so the time of the other frames is
/// extrapolated from the length of the frames: the clock counts units (bits of the coded data, or
/// samples for streams with a fixed number of samples in a frame) at a rate of `rate` units per
/// second, from the last time stamp of the container that anchored it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Clock {
    anchor: i64,
    units: u64,
    rate: u64,
}

impl Clock {
    pub fn new(rate: u32) -> Self {
        Clock { anchor: 0, units: 0, rate: u64::from(rate) }
    }

    /// The time in milliseconds of `units`.
    fn ms(&self, units: u64) -> i64 {
        if self.rate == 0 {
            return 0;
        }

        (u128::from(units) * 1000 / u128::from(self.rate)).min(i64::MAX as u128) as i64
    }

    /// Anchor the clock at a time stamp of the container.
    pub fn anchor(&mut self, ts: i64) {
        self.anchor = ts;
        self.units = 0;
    }

    /// Get the time stamp and duration in milliseconds of a frame of `units`, and advance the clock.
    fn stamp(&mut self, units: u64) -> (i64, u64) {
        let start = self.ms(self.units);
        let end = self.ms(self.units.saturating_add(units));

        self.units = self.units.saturating_add(units);

        (self.anchor.saturating_add(start), end.saturating_sub(start).max(0) as u64)
    }
}

/// An audio stream of the file.
pub(crate) struct AudioStream {
    pub track_id: u32,
    pub info: AudioInfo,
    /// The positions and time stamps of the packets that decoding can start at, increasing.
    pub index: Vec<(i64, u64)>,
    /// True if packets are skipped until the start of a group (after a seek).
    pub need_sync: bool,
    clock: Clock,
    /// The group of frames being assembled.
    group: Vec<u8>,
    /// The number of packets of the container in the group.
    group_cnt: usize,
    /// The time stamp of the first packet of the group.
    group_ts: Option<i64>,
    /// The parser of the frames of an AC-3 stream.
    es: Option<Box<dyn EsParser>>,
    /// True if the clock is to be anchored at the next time stamp of the container.
    anchor_next: bool,
    /// The number of bytes of the frames of the stream, if the packets are split into frames.
    frame_len: usize,
    /// The number of units in a frame that does not depend on its length (0 if it does).
    frame_units: u64,
    scratch: Vec<u8>,
}

impl AudioStream {
    pub fn new(track_id: u32, info: AudioInfo) -> Self {
        let mut es: Option<Box<dyn EsParser>> = None;
        let mut frame_units = 0;

        let rate = match info.codec {
            Some(CODEC_ID_AAC) => {
                // A frame is 1024 samples of the sample rate of the core audio.
                frame_units = AAC_FRAME_LEN;

                info.extra
                    .as_deref()
                    .and_then(|extra| AudioSpecificConfig::read(extra).ok())
                    .map_or(info.sample_rate, |asc| asc.sample_rate)
            }
            Some(CODEC_ID_AC3) => {
                es = Some(Box::new(Ac3Es::new()));
                info.sample_rate
            }
            _ => info.bit_rate,
        };

        let frame_len = match info.codec {
            Some(CODEC_ID_RA10) => RA144_FRAME_LEN,
            Some(CODEC_ID_RA20 | CODEC_ID_COOK | CODEC_ID_ATRAC3 | CODEC_ID_SIPR) => {
                info.block_align
            }
            _ => 0,
        };

        let group =
            if info.is_grouped() { vec![0; info.frame_size * info.sub_packet_h] } else { vec![] };

        AudioStream {
            track_id,
            info,
            index: Vec::new(),
            need_sync: false,
            clock: Clock::new(rate),
            group,
            group_cnt: 0,
            group_ts: None,
            es,
            anchor_next: true,
            frame_len,
            frame_units,
            scratch: Vec::new(),
        }
    }

    pub fn clock_mut(&mut self) -> &mut Clock {
        &mut self.clock
    }

    /// Returns true if the container packet with `flags` is a point that decoding can start at.
    pub fn is_sync_point(&self, flags: u8) -> bool {
        !self.info.is_grouped() || flags & FLAG_KEY != 0
    }

    /// Discard the packets that are being assembled (after a seek).
    pub fn reset(&mut self) {
        self.group_cnt = 0;
        self.group_ts = None;
        self.need_sync = self.info.is_grouped();
        self.anchor_next = true;

        if let Some(es) = self.es.as_mut() {
            es.clear();
        }
    }

    /// The number of units of a frame of `len` bytes.
    fn units_of(&self, len: usize) -> u64 {
        if self.frame_units > 0 { self.frame_units } else { len as u64 * 8 }
    }

    /// Process the payload of a packet of the container. `ts` is the time stamp of the packet in
    /// milliseconds, absent for `.ra` files. The frames are appended to `out`.
    pub fn process(&mut self, data: &[u8], ts: Option<i64>, flags: u8, out: &mut VecDeque<Packet>) {
        match self.info.interleave {
            Interleave::Genr | Interleave::Int4 | Interleave::Sipr => {
                self.process_group(data, ts, flags, out)
            }
            Interleave::Vbr => self.process_vbr(data, ts, out),
            Interleave::None => self.process_raw(data, ts, out),
        }
    }

    /// Flush the frames that the parser of the stream has not yet released.
    pub fn flush(&mut self, out: &mut VecDeque<Packet>) {
        self.drain_es(true, out);
    }

    fn emit(&mut self, data: &[u8], units: u64, out: &mut VecDeque<Packet>) {
        let (pts, dur) = self.clock.stamp(units);

        let mut packet = Packet::new(self.track_id, Timestamp::new(pts), Duration::new(dur), data);
        packet.dts = Timestamp::new(pts);

        out.push_back(packet);
    }

    fn process_group(
        &mut self,
        data: &[u8],
        ts: Option<i64>,
        flags: u8,
        out: &mut VecDeque<Packet>,
    ) {
        let (w, h) = (self.info.frame_size, self.info.sub_packet_h);
        let (sps, cfs) = (self.info.sub_packet_size, self.info.coded_frame_size);

        if flags & FLAG_KEY != 0 {
            // A key frame starts a new group.
            self.group_cnt = 0;
            self.need_sync = false;
        }
        else if self.need_sync {
            return;
        }

        let y = self.group_cnt;

        if y == 0 {
            self.group_ts = ts;
        }

        // Place the data of the packet in the group.
        let placed = match self.info.interleave {
            Interleave::Int4 => (0..h / 2).all(|x| {
                let (src, dst) = (x * cfs, x * 2 * w + y * cfs);
                copy_part(data, src, &mut self.group, dst, cfs)
            }),
            Interleave::Genr => (0..w / sps).all(|x| {
                let dst = sps * (h * x + h.div_ceil(2) * (y & 1) + (y >> 1));
                copy_part(data, x * sps, &mut self.group, dst, sps)
            }),
            _ => copy_part(data, 0, &mut self.group, y * w, w),
        };

        if !placed {
            warn!("real: packet is too short ({} bytes)", data.len());
            return;
        }

        self.group_cnt += 1;

        if self.group_cnt < h {
            return;
        }

        self.group_cnt = 0;

        if self.info.interleave == Interleave::Sipr {
            reorder_sipr(&mut self.group, h, w);
        }

        if let Some(ts) = self.group_ts.take() {
            self.clock.anchor(ts);
        }

        let ba = self.info.block_align;
        let units = self.units_of(ba);
        let group = std::mem::take(&mut self.group);

        for frame in group.chunks_exact(ba) {
            self.emit(frame, units, out);
        }

        self.group = group;
    }

    fn process_vbr(&mut self, data: &[u8], ts: Option<i64>, out: &mut VecDeque<Packet>) {
        let Some(&[b0, b1]) = data.first_chunk::<2>()
        else {
            return;
        };

        let count = usize::from((u16::from_be_bytes([b0, b1]) & 0xf0) >> 4);

        if count == 0 {
            return;
        }

        let Some(lens) = data.get(2..2 + 2 * count)
        else {
            warn!("real: frame length table is truncated");
            return;
        };

        if let Some(ts) = ts {
            self.clock.anchor(ts);
        }

        let mut pos = 2 + 2 * count;

        for len in lens.chunks_exact(2) {
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));

            let Some(frame) = data.get(pos..pos + len)
            else {
                warn!("real: frame is truncated");
                return;
            };

            let units = self.units_of(len);
            self.emit(frame, units, out);
            pos += len;
        }
    }

    fn process_raw(&mut self, data: &[u8], ts: Option<i64>, out: &mut VecDeque<Packet>) {
        if self.es.is_some() {
            return self.process_ac3(data, ts, out);
        }

        if let Some(ts) = ts {
            self.clock.anchor(ts);
        }

        if self.frame_len == 0 {
            let units = self.units_of(data.len());
            self.emit(data, units, out);
            return;
        }

        let units = self.units_of(self.frame_len);

        for frame in data.chunks_exact(self.frame_len) {
            self.emit(frame, units, out);
        }
    }

    /// An AC-3 (`dnet`) packet has the bytes of every 16 bit word swapped. The frames are found
    /// by the parser, as the packets do not hold whole frames.
    fn process_ac3(&mut self, data: &[u8], ts: Option<i64>, out: &mut VecDeque<Packet>) {
        let mut swapped = std::mem::take(&mut self.scratch);

        swapped.clear();
        swapped.extend_from_slice(data);

        for pair in swapped.chunks_exact_mut(2) {
            pair.swap(0, 1);
        }

        if self.anchor_next {
            self.anchor_next = false;

            if let Some(ts) = ts {
                self.clock.anchor(ts);
            }
        }

        if let Some(es) = self.es.as_mut() {
            es.push(&swapped);
        }

        self.scratch = swapped;
        self.drain_es(false, out);
    }

    fn drain_es(&mut self, flush: bool, out: &mut VecDeque<Packet>) {
        while let Some(frame) = self.es.as_mut().and_then(|es| es.next_frame(flush)) {
            if let Some(info) = self.es.as_ref().and_then(|es| es.info())
                && info.rate != 0
                && u64::from(info.rate) != self.clock.rate
            {
                debug!("real: AC-3 sample rate is {}", info.rate);
                self.clock.rate = u64::from(info.rate);
            }

            self.emit(&frame.data, frame.dur, out);
        }
    }
}

/// Copy `len` bytes of `src` at `src_pos` to `dst` at `dst_pos`. Returns false if either does not
/// have the bytes.
fn copy_part(src: &[u8], src_pos: usize, dst: &mut [u8], dst_pos: usize, len: usize) -> bool {
    let (Some(from), Some(to)) = (
        src.get(src_pos..src_pos.saturating_add(len)),
        dst.get_mut(dst_pos..dst_pos.saturating_add(len)),
    )
    else {
        return false;
    };

    to.copy_from_slice(from);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_sipr_reorder_is_an_involution() {
        let orig: Vec<u8> = (0..96 * 2).map(|i| (i * 37 + 11) as u8).collect();
        let mut buf = orig.clone();

        reorder_sipr(&mut buf, 6, 32);
        assert_ne!(buf, orig);
        reorder_sipr(&mut buf, 6, 32);
        assert_eq!(buf, orig);
    }

    #[test]
    fn verify_clock_is_exact_over_frames() {
        // 20 byte frames at 8000 bits per second: 20 ms each.
        let mut clock = Clock::new(8000);
        clock.anchor(100);

        assert_eq!(clock.stamp(160), (100, 20));
        assert_eq!(clock.stamp(160), (120, 20));

        // 7 byte frames at 8000 bits per second: no accumulated rounding error.
        let mut clock = Clock::new(8000);
        let total: u64 = (0..1000).map(|_| clock.stamp(56).1).sum();
        assert_eq!(total, 7000);
    }

    #[test]
    fn verify_genr_deinterleave() {
        // 4 packets of 2 sub packets of 1 byte: frame_size 2, h 4, sps 1, block_align 1.
        let info = AudioInfo {
            codec: Some(CODEC_ID_COOK),
            sample_rate: 8000,
            channels: 1,
            bit_rate: 8000,
            extra: None,
            interleave: Interleave::Genr,
            frame_size: 2,
            sub_packet_h: 4,
            sub_packet_size: 1,
            coded_frame_size: 0,
            block_align: 1,
            text: vec![],
        };

        let mut stream = AudioStream::new(1, info);
        let mut out = VecDeque::new();

        for (y, data) in [[0u8, 4], [1, 5], [2, 6], [3, 7]].iter().enumerate() {
            stream.process(data, Some(10), if y == 0 { FLAG_KEY } else { 0 }, &mut out);
        }

        let got: Vec<u8> = out.iter().map(|p| p.data[0]).collect();
        // ffmpeg: offset = sps * (h * x + ((h + 1) / 2) * (y & 1) + (y >> 1)).
        assert_eq!(got, [0, 2, 1, 3, 4, 6, 5, 7]);
        assert_eq!(out[0].pts.get(), 10);
    }
}
