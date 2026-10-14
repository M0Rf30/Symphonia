// Symphonia DSD Codec
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Direct Stream Transfer (DST) decoder.
//!
//! DST is the lossless compression of 1-bit DSD audio that is used by the Super Audio CD and by
//! compressed DSDIFF files. It is specified by ISO/IEC 14496-3, Subpart 10 ("Lossless coding of
//! oversampled audio"). This is an independent implementation of the decoder in that
//! specification, validated bit for bit against reference decoders.
//!
//! A DST frame holds 1/75 s of audio of all channels (at most 6). It is either a raw copy of the
//! DSD data (`DSTCoded` = 0), or, for each channel, the residual of a predictor that is coded with
//! an adaptive binary arithmetic code:
//!
//! 1. The frame header describes how the frame is divided in time into segments (up to 4 for the
//!    filters and 8 for the probability tables per channel), which of the filter and probability
//!    tables each segment uses, the coefficients of those filters (a 1-bit-input FIR predictor of
//!    up to 128 taps) and the probability tables, both optionally Rice coded.
//! 2. For each DSD sample, the filter output `predict` for the 128 previous samples of the
//!    channel selects an entry of a probability table, `|predict| >> 3`, which is the probability
//!    for the arithmetic decoder to decode a bit that tells whether the sample was predicted
//!    correctly. The predicted sample is the sign of `predict`.
//!
//! Every frame is independent: the history of the predictors starts as the pattern `0xAA` in each
//! frame. A frame can therefore be decoded without any other frame.

use symphonia_core::errors::{Result, decode_error};

/// The largest number of channels in a DST stream.
pub const DST_MAX_CHANNELS: usize = 6;

const MAX_TABLES: usize = 2 * DST_MAX_CHANNELS;
const MAX_FILTER_SEGMENTS: usize = 4;
const MAX_PTABLE_SEGMENTS: usize = 8;
const MAX_SEGMENTS: usize = MAX_PTABLE_SEGMENTS;
const MIN_FILTER_SEGMENT_BITS: usize = 1024;
const MIN_PTABLE_SEGMENT_BITS: usize = 32;
const MAX_PTABLE_LEN: usize = 64;

/// The number of bits for the order of a filter: 1 to 128 taps.
const FILTER_ORDER_BITS: u32 = 7;
/// The number of bits of a filter coefficient (two's complement).
const FILTER_COEF_BITS: u32 = 9;
/// The number of bits for the length of a probability table: 1 to 64 entries.
const PTABLE_LEN_BITS: u32 = 6;
/// The number of bits of a probability in a probability table, minus 1 (the probability is 1 to
/// 128, in units of 1/256).
const PTABLE_ENTRY_BITS: u32 = 7;
/// The number of bits that select the prediction method of a Rice coded table.
const RICE_METHOD_BITS: u32 = 2;
/// The number of bits of the Rice parameter `m` of a Rice coded table.
const RICE_M_BITS: u32 = 3;
/// The probability table index is `|predict| >> PTABLE_SHIFT`.
const PTABLE_SHIFT: u32 = 3;

/// The coefficients with which the Rice coded tables predict an entry from the preceding
/// entries, for each of the three methods (ISO/IEC 14496-3 Subpart 10, 10.12 and 10.13).
const FILTER_RICE_PREDICTION: [&[i32]; 3] = [&[-8], &[-16, 8], &[-9, -5, 6]];
const PTABLE_RICE_PREDICTION: [&[i32]; 3] = [&[-8], &[-16, 8], &[-24, 24, -8]];

/// The history of a channel at the start of each frame.
const INITIAL_HISTORY: u128 = 0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA;

/// The padding of a raw frame that is shorter than a frame of DSD, the DSD idle pattern.
const DSD_IDLE: u8 = 0x69;

/// The CRC polynomial of the DST frame CRC chunk (DSDIFF 1.5, 3.4.3):
/// `G(x) = x^32 + x^31 + x^4 + 1`, without the `x^32` term.
const CRC_POLYNOMIAL: u32 = 0x8000_0011;

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ CRC_POLYNOMIAL } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = build_crc_table();

/// Compute the CRC of a DST frame as defined for the DSDIFF DST frame CRC chunk (`DSTC`): the
/// remainder of the division by `G(x)` of the bits of the decoded, channel-interleaved DSD frame
/// (most significant bit first) followed by 32 zero bits.
pub fn dst_frame_crc(dsd: &[u8]) -> u32 {
    dsd.iter()
        .fold(0u32, |crc, &byte| (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ byte)])
}

/// The number of bits needed to represent `x` (`Log2RoundUp` in the specification).
fn bit_length(x: usize) -> u32 {
    usize::BITS - x.leading_zeros()
}

/// A big-endian bit reader. Reading past the end of the data yields zero bits, as defined for the
/// arithmetic code, and is recorded.
struct BitReader<'a> {
    data: &'a [u8],
    /// The index of the next byte to load.
    next: usize,
    cache: u64,
    cached: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, next: 0, cache: 0, cached: 0 }
    }

    /// The number of bits read so far.
    fn position(&self) -> usize {
        self.next * 8 - self.cached as usize
    }

    /// True if bits past the end of the data have been read.
    fn overrun(&self) -> bool {
        self.position() > self.data.len() * 8
    }

    fn refill(&mut self) {
        while self.cached <= 56 {
            let byte = self.data.get(self.next).copied().unwrap_or(0);
            self.next += 1;
            self.cache |= u64::from(byte) << (56 - self.cached);
            self.cached += 8;
        }
    }

    /// Read `n` bits, `n <= 32`.
    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        if self.cached < n {
            self.refill();
        }
        let value = (self.cache >> (64 - n)) as u32;
        self.cache <<= n;
        self.cached -= n;
        value
    }

    fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Read an `n`-bit two's complement number.
    fn read_signed(&mut self, n: u32) -> i32 {
        let v = self.read(n) as i32;
        (v << (32 - n)) >> (32 - n)
    }

    /// Read a Rice code with parameter `m`: a unary run length, `m` least significant bits, and a
    /// sign bit for a non-zero value.
    fn read_rice(&mut self, m: u32) -> i32 {
        let mut run = 0u64;
        while !self.bit() {
            run += 1;
            if self.overrun() {
                break;
            }
        }
        let value = ((run << m) + u64::from(self.read(m))).min(1 << 24) as i32;
        if value != 0 && self.bit() { -value } else { value }
    }
}

/// How a frame is divided in time for the filters or the probability tables.
#[derive(Clone, Default)]
struct Segmentation {
    /// The segment lengths are multiples of `resolution` bytes.
    resolution: usize,
    /// The number of segments of each channel.
    count: [usize; DST_MAX_CHANNELS],
    /// The length of each segment, in units of the resolution. The last segment of a channel
    /// extends to the end of the frame.
    length: [[usize; MAX_SEGMENTS]; DST_MAX_CHANNELS],
    /// The table each segment uses.
    table: [[usize; MAX_SEGMENTS]; DST_MAX_CHANNELS],
    /// The number of tables.
    num_tables: usize,
}

impl Segmentation {
    /// The index of the bit (DSD sample) at which segment `segment` of `channel` ends, or the end
    /// of the frame for the last segment.
    fn end_bit(&self, channel: usize, segment: usize, frame_bits: usize) -> usize {
        if segment + 1 >= self.count[channel] {
            frame_bits
        }
        else {
            // The sum of the lengths of the segments up to and including this one.
            let units: usize = self.length[channel][..=segment].iter().sum();
            (units * self.resolution * 8).min(frame_bits)
        }
    }
}

/// Reads the segment lengths of one channel (or of all channels).
struct SegmentReader {
    frame_len: usize,
    min_bits: usize,
    resolution: usize,
    defined_bits: usize,
    max_size: usize,
}

impl SegmentReader {
    fn new(frame_len: usize, min_bits: usize) -> Self {
        let mut reader =
            SegmentReader { frame_len, min_bits, resolution: 0, defined_bits: 0, max_size: 0 };
        reader.next_channel();
        reader
    }

    /// The largest resolution and segment size, in bytes.
    fn max_bytes(&self) -> usize {
        self.frame_len.saturating_sub(self.min_bits / 8)
    }

    fn next_channel(&mut self) {
        self.defined_bits = 0;
        self.max_size = self.max_bytes();
    }

    /// Read the resolution, if not yet read, and the length of a segment.
    fn read_length(&mut self, r: &mut BitReader<'_>) -> Result<usize> {
        if self.resolution == 0 {
            let resolution = r.read(bit_length(self.max_bytes())) as usize;
            if resolution == 0 || resolution > self.max_bytes() {
                return decode_error("dst: invalid segment resolution");
            }
            self.resolution = resolution;
        }
        let length = r.read(bit_length(self.max_size / self.resolution)) as usize;
        let bits = self.resolution * 8 * length;
        let room = (self.frame_len * 8).saturating_sub(self.defined_bits + self.min_bits);
        if bits < self.min_bits || bits > room {
            return decode_error("dst: invalid segment length");
        }
        self.defined_bits += bits;
        self.max_size = self.max_size.saturating_sub(self.resolution * length);
        Ok(length)
    }
}

/// Arithmetic decoder (ISO/IEC 14496-3 Subpart 10, 10.11). The `a` register has 12 bits and the
/// `c` register is kept to 12 bits by the normalization.
struct ArithmeticDecoder {
    a: u32,
    c: u32,
}

impl ArithmeticDecoder {
    fn new(r: &mut BitReader<'_>) -> Self {
        ArithmeticDecoder { a: 4095, c: r.read(12) }
    }

    /// Decode a bit, given the probability `p` (1 to 128, of 256) that it is 1.
    #[inline]
    fn decode(&mut self, r: &mut BitReader<'_>, p: u32) -> u32 {
        // The product of `a` and `p` is approximated with partial rounding.
        let k = (self.a >> 8) | ((self.a >> 7) & 1);
        let q = k * p;
        let a_q = self.a - q;
        let bit = u32::from(self.c < a_q);
        if bit != 0 {
            self.a = a_q;
        }
        else {
            self.a = q;
            self.c -= a_q;
        }
        if self.a < 2048 {
            let n = self.a.leading_zeros() - 20;
            self.a <<= n;
            self.c = (self.c << n) | r.read(n);
        }
        bit
    }
}

/// A DST decoder for streams of a given channel count and frame length.
pub struct DstDecoder {
    channels: usize,
    /// The length in bytes of the decoded frame of one channel.
    frame_len: usize,
    /// For each filter, the 16 tables of the outputs for all values of 8 history bits.
    filters: Vec<[[i16; 256]; 16]>,
    filter_order: [usize; MAX_TABLES],
    ptables: [[u8; MAX_PTABLE_LEN]; MAX_TABLES],
    ptable_len: [usize; MAX_TABLES],
    filter_segments: Segmentation,
    ptable_segments: Segmentation,
    half_prob: [bool; DST_MAX_CHANNELS],
    /// The first coefficient of the first filter, which sets the probability of the first bit.
    first_coefficient: i32,
}

impl DstDecoder {
    /// Create a decoder for `channels` channels of frames of `frame_len` bytes per channel.
    pub fn new(channels: usize, frame_len: usize) -> Result<Self> {
        if channels == 0 || channels > DST_MAX_CHANNELS {
            return decode_error("dst: unsupported channel count");
        }
        if frame_len == 0 {
            return decode_error("dst: invalid frame length");
        }
        Ok(DstDecoder {
            channels,
            frame_len,
            filters: vec![[[0; 256]; 16]; MAX_TABLES],
            filter_order: [0; MAX_TABLES],
            ptables: [[0; MAX_PTABLE_LEN]; MAX_TABLES],
            ptable_len: [0; MAX_TABLES],
            filter_segments: Segmentation::default(),
            ptable_segments: Segmentation::default(),
            half_prob: [false; DST_MAX_CHANNELS],
            first_coefficient: 0,
        })
    }

    /// The length in bytes of a decoded frame: all channels, interleaved by byte.
    pub fn frame_bytes(&self) -> usize {
        self.frame_len * self.channels
    }

    /// Decode a DST frame to `out`, which is `frame_bytes()` long, as channel-interleaved DSD,
    /// most significant bit first.
    ///
    /// If `strict`, a frame is also rejected if its arithmetic code does not end where the
    /// specification demands, which detects most corruption that does not otherwise make the
    /// frame invalid.
    pub fn decode(&mut self, frame: &[u8], out: &mut [u8], strict: bool) -> Result<()> {
        if out.len() != self.frame_bytes() {
            return decode_error("dst: output buffer has the wrong length");
        }
        if frame.is_empty() {
            return decode_error("dst: empty frame");
        }
        let mut r = BitReader::new(frame);

        // 10.3: the frame is either DST coded or a copy of the DSD data.
        if !r.bit() {
            r.read(1);
            if r.read(6) != 0 {
                return decode_error("dst: invalid stuffing pattern");
            }
            let raw = &frame[1..];
            let n = raw.len().min(out.len());
            out[..n].copy_from_slice(&raw[..n]);
            out[n..].fill(DSD_IDLE);
            return Ok(());
        }

        self.read_segmentation(&mut r)?;
        self.read_mapping(&mut r)?;
        self.read_filters(&mut r)?;
        self.read_ptables(&mut r)?;
        if r.overrun() {
            return decode_error("dst: frame ends inside its header");
        }

        // 10.11: arithmetic coded data. The first bit is always 0.
        if r.bit() {
            return decode_error("dst: invalid arithmetic code");
        }
        self.decode_samples(&mut r, out);

        // The decoder ends after the last bit of the arithmetic code, give or take up to 7 bits of
        // padding to a whole byte (10.11).
        if strict && r.position() + 7 < frame.len() * 8 {
            return decode_error("dst: arithmetic code is shorter than the frame data");
        }
        Ok(())
    }

    /// Read the segmentation of the filters and probability tables (10.4 to 10.6).
    fn read_segmentation(&mut self, r: &mut BitReader<'_>) -> Result<()> {
        let same = r.bit();
        Self::read_segments(
            r,
            &mut self.filter_segments,
            self.channels,
            self.frame_len,
            MAX_FILTER_SEGMENTS,
            MIN_FILTER_SEGMENT_BITS,
        )?;
        if same {
            // The probability tables are segmented like the filters.
            self.ptable_segments = self.filter_segments.clone();
            let seg = &self.ptable_segments;
            for ch in 0..self.channels {
                for s in 0..seg.count[ch] {
                    let len = seg.length[ch][s];
                    if len != 0 && seg.resolution * 8 * len < MIN_PTABLE_SEGMENT_BITS {
                        return decode_error("dst: invalid segment length");
                    }
                }
            }
            Ok(())
        }
        else {
            Self::read_segments(
                r,
                &mut self.ptable_segments,
                self.channels,
                self.frame_len,
                MAX_PTABLE_SEGMENTS,
                MIN_PTABLE_SEGMENT_BITS,
            )
        }
    }

    fn read_segments(
        r: &mut BitReader<'_>,
        seg: &mut Segmentation,
        channels: usize,
        frame_len: usize,
        max_segments: usize,
        min_bits: usize,
    ) -> Result<()> {
        *seg = Segmentation::default();
        let mut reader = SegmentReader::new(frame_len, min_bits);

        if r.bit() {
            // All channels are segmented alike.
            let mut n = 0;
            while !r.bit() {
                if n >= max_segments {
                    return decode_error("dst: too many segments");
                }
                seg.length[0][n] = reader.read_length(r)?;
                n += 1;
            }
            if n + 1 > max_segments {
                return decode_error("dst: too many segments");
            }
            seg.count[0] = n + 1;
            for ch in 1..channels {
                seg.count[ch] = seg.count[0];
                seg.length[ch] = seg.length[0];
            }
        }
        else {
            let (mut ch, mut n) = (0, 0);
            while ch < channels {
                if n >= max_segments {
                    return decode_error("dst: too many segments");
                }
                if r.bit() {
                    seg.count[ch] = n + 1;
                    n = 0;
                    ch += 1;
                    reader.next_channel();
                }
                else {
                    seg.length[ch][n] = reader.read_length(r)?;
                    n += 1;
                }
            }
        }
        seg.resolution = reader.resolution.max(1);
        if r.overrun() {
            return decode_error("dst: frame ends inside its header");
        }
        Ok(())
    }

    /// Read which filter and which probability table each segment uses (10.7 to 10.10), and which
    /// channels use half probability at the start of the frame.
    fn read_mapping(&mut self, r: &mut BitReader<'_>) -> Result<()> {
        let same = r.bit();
        Self::read_table_numbers(r, &mut self.filter_segments, self.channels, 2 * self.channels)?;
        if same {
            for ch in 0..self.channels {
                if self.ptable_segments.count[ch] != self.filter_segments.count[ch] {
                    return decode_error("dst: segment count mismatch");
                }
            }
            self.ptable_segments.table = self.filter_segments.table;
            self.ptable_segments.num_tables = self.filter_segments.num_tables;
        }
        else {
            Self::read_table_numbers(
                r,
                &mut self.ptable_segments,
                self.channels,
                2 * self.channels,
            )?;
        }
        for ch in 0..self.channels {
            self.half_prob[ch] = r.bit();
        }
        Ok(())
    }

    fn read_table_numbers(
        r: &mut BitReader<'_>,
        seg: &mut Segmentation,
        channels: usize,
        max_tables: usize,
    ) -> Result<()> {
        let mut tables = 1;
        seg.table[0][0] = 0;
        let mut read_number = |r: &mut BitReader<'_>| -> Result<usize> {
            let number = r.read(bit_length(tables)) as usize;
            if number == tables {
                tables += 1;
            }
            else if number > tables {
                return decode_error("dst: invalid table number");
            }
            Ok(number)
        };

        if r.bit() {
            // All channels use the same tables in the same segments.
            for s in 1..seg.count[0] {
                seg.table[0][s] = read_number(r)?;
            }
            for ch in 1..channels {
                if seg.count[ch] != seg.count[0] {
                    return decode_error("dst: invalid channel mapping");
                }
                seg.table[ch] = seg.table[0];
            }
        }
        else {
            for ch in 0..channels {
                for s in 0..seg.count[ch] {
                    if ch != 0 || s != 0 {
                        seg.table[ch][s] = read_number(r)?;
                    }
                }
            }
        }
        if tables > max_tables {
            return decode_error("dst: too many tables");
        }
        seg.num_tables = tables;
        if r.overrun() {
            return decode_error("dst: frame ends inside its header");
        }
        Ok(())
    }

    /// Read the filter coefficients (10.12) and build the filter lookup tables.
    fn read_filters(&mut self, r: &mut BitReader<'_>) -> Result<()> {
        let mut coefs = [0i32; 128];
        for f in 0..self.filter_segments.num_tables {
            let order = r.read(FILTER_ORDER_BITS) as usize + 1;
            if !r.bit() {
                for coef in &mut coefs[..order] {
                    *coef = r.read_signed(FILTER_COEF_BITS);
                }
            }
            else {
                let method = r.read(RICE_METHOD_BITS) as usize;
                if method >= FILTER_RICE_PREDICTION.len() {
                    return decode_error("dst: invalid filter coefficient coding");
                }
                let prediction = FILTER_RICE_PREDICTION[method];
                if prediction.len() >= order {
                    return decode_error("dst: invalid filter coefficient coding");
                }
                for coef in &mut coefs[..prediction.len()] {
                    *coef = r.read_signed(FILTER_COEF_BITS);
                }
                let m = r.read(RICE_M_BITS);
                for j in prediction.len()..order {
                    let c = Self::read_rice_entry(r, &coefs, j, prediction, m);
                    if !(-(1 << (FILTER_COEF_BITS - 1))..(1 << (FILTER_COEF_BITS - 1))).contains(&c)
                    {
                        return decode_error("dst: filter coefficient out of range");
                    }
                    coefs[j] = c;
                }
            }
            if r.overrun() {
                return decode_error("dst: frame ends inside its header");
            }
            self.filter_order[f] = order;
            if f == 0 {
                self.first_coefficient = coefs[0];
            }
            Self::build_filter(&mut self.filters[f], &coefs[..order]);
        }
        Ok(())
    }

    /// Decode entry `j` of a Rice coded table, which predicts it from the preceding entries.
    fn read_rice_entry(
        r: &mut BitReader<'_>,
        table: &[i32],
        j: usize,
        prediction: &[i32],
        m: u32,
    ) -> i32 {
        let x: i32 = prediction.iter().enumerate().map(|(t, &p)| p * table[j - t - 1]).sum();
        let residual = r.read_rice(m);
        if x >= 0 { residual - (x + 4) / 8 } else { residual + (-x + 3) / 8 }
    }

    /// Build the lookup tables of a filter: table `g` holds the filter output of the taps
    /// `8g..8g+8` for each value of the 8 history bits they apply to, where a 1 bit counts as
    /// `+coefficient` and a 0 bit as `-coefficient`.
    fn build_filter(tables: &mut [[i16; 256]; 16], coefs: &[i32]) {
        for (g, table) in tables.iter_mut().enumerate() {
            let mut taps = [0i32; 8];
            let first = (g * 8).min(coefs.len());
            let last = (g * 8 + 8).min(coefs.len());
            taps[..last - first].copy_from_slice(&coefs[first..last]);
            let total: i32 = taps.iter().sum();

            // sums[k] is the sum of the coefficients of the set bits of k.
            let mut sums = [0i32; 256];
            for k in 1..256usize {
                sums[k] = sums[k & (k - 1)] + taps[k.trailing_zeros() as usize];
                table[k] = (2 * sums[k] - total) as i16;
            }
            table[0] = (-total) as i16;
        }
    }

    /// Read the probability tables (10.13).
    fn read_ptables(&mut self, r: &mut BitReader<'_>) -> Result<()> {
        for p in 0..self.ptable_segments.num_tables {
            let len = r.read(PTABLE_LEN_BITS) as usize + 1;
            self.ptable_len[p] = len;
            let mut entries = [0i32; MAX_PTABLE_LEN];
            if len == 1 {
                entries[0] = 128;
            }
            else if !r.bit() {
                for entry in &mut entries[..len] {
                    *entry = r.read(PTABLE_ENTRY_BITS) as i32 + 1;
                }
            }
            else {
                let method = r.read(RICE_METHOD_BITS) as usize;
                if method >= PTABLE_RICE_PREDICTION.len() {
                    return decode_error("dst: invalid probability table coding");
                }
                let prediction = PTABLE_RICE_PREDICTION[method];
                if prediction.len() >= len {
                    return decode_error("dst: invalid probability table coding");
                }
                for entry in &mut entries[..prediction.len()] {
                    *entry = r.read(PTABLE_ENTRY_BITS) as i32 + 1;
                }
                let m = r.read(RICE_M_BITS);
                for j in prediction.len()..len {
                    let c = Self::read_rice_entry(r, &entries, j, prediction, m);
                    if !(1..=128).contains(&c) {
                        return decode_error("dst: probability out of range");
                    }
                    entries[j] = c;
                }
            }
            if r.overrun() {
                return decode_error("dst: frame ends inside its header");
            }
            for (dst, &src) in self.ptables[p].iter_mut().zip(&entries[..len]) {
                *dst = src as u8;
            }
        }
        Ok(())
    }

    /// Decode the DSD samples of all channels from the arithmetic code (10.11 and 10.14).
    fn decode_samples(&mut self, r: &mut BitReader<'_>, out: &mut [u8]) {
        let channels = self.channels;
        let frame_bits = self.frame_len * 8;
        out.fill(0);

        let mut ac = ArithmeticDecoder::new(r);

        // The first decoded bit is not part of the audio: its probability depends on the first
        // filter coefficient.
        let first = self.first_coefficient as u32 & 127;
        let _ = ac.decode(r, (first.reverse_bits() >> 25) + 1);

        let mut history = [INITIAL_HISTORY; DST_MAX_CHANNELS];

        // The current filter and probability table segment of each channel, and where they end.
        let mut filter_segment = [0usize; DST_MAX_CHANNELS];
        let mut filter_end = [0usize; DST_MAX_CHANNELS];
        let mut ptable_segment = [0usize; DST_MAX_CHANNELS];
        let mut ptable_end = [0usize; DST_MAX_CHANNELS];
        let mut half_bits = [0usize; DST_MAX_CHANNELS];
        for ch in 0..channels {
            filter_end[ch] = self.filter_segments.end_bit(ch, 0, frame_bits);
            ptable_end[ch] = self.ptable_segments.end_bit(ch, 0, frame_bits);
            // Half probability applies to as many bits as the first filter of the channel has taps.
            half_bits[ch] = if self.half_prob[ch] {
                self.filter_order[self.filter_segments.table[ch][0]]
            }
            else {
                0
            };
        }

        for i in 0..frame_bits {
            let byte = (i >> 3) * channels;
            let shift = 7 - (i & 7) as u32;
            for ch in 0..channels {
                if i == filter_end[ch] {
                    filter_segment[ch] += 1;
                    filter_end[ch] =
                        self.filter_segments.end_bit(ch, filter_segment[ch], frame_bits);
                }
                if i == ptable_end[ch] {
                    ptable_segment[ch] += 1;
                    ptable_end[ch] =
                        self.ptable_segments.end_bit(ch, ptable_segment[ch], frame_bits);
                }

                let filter = &self.filters[self.filter_segments.table[ch][filter_segment[ch]]];
                let h = history[ch];
                let mut sum = 0i32;
                for (g, table) in filter.iter().enumerate() {
                    sum += i32::from(table[((h >> (8 * g)) & 0xFF) as usize]);
                }
                // The filter output is a 16-bit quantity.
                let predict = sum as i16;

                let p = if i < half_bits[ch] {
                    128
                }
                else {
                    let table = self.ptable_segments.table[ch][ptable_segment[ch]];
                    let index = (usize::from(predict.unsigned_abs()) >> PTABLE_SHIFT)
                        .min(self.ptable_len[table] - 1);
                    u32::from(self.ptables[table][index])
                };

                // The arithmetic code tells whether the sign of the prediction is wrong.
                let residual = ac.decode(r, p);
                let bit = (u32::from(predict < 0) ^ residual) & 1;
                out[byte + ch] |= (bit as u8) << shift;
                history[ch] = (h << 1) | u128::from(bit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_crc_of_zero_data_is_zero() {
        assert_eq!(dst_frame_crc(&[0; 100]), 0);
    }

    #[test]
    fn verify_crc_matches_bitwise_division() {
        // The remainder of x^32 * I(x) divided by G(x), computed one bit at a time.
        fn reference(data: &[u8]) -> u32 {
            let mut crc = 0u32;
            for &byte in data {
                for bit in (0..8).rev() {
                    let input = u32::from(byte >> bit) & 1;
                    let top = crc >> 31;
                    crc <<= 1;
                    if top ^ input != 0 {
                        crc ^= CRC_POLYNOMIAL;
                    }
                }
            }
            crc
        }
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + i / 3) as u8).collect();
        assert_eq!(dst_frame_crc(&data), reference(&data));
    }

    #[test]
    fn verify_bit_reader() {
        let mut r = BitReader::new(&[0b1010_0000, 0xFF, 0x0F]);
        assert!(r.bit());
        assert_eq!(r.read(2), 0b01);
        assert_eq!(r.read(5), 0);
        assert_eq!(r.position(), 8);
        assert_eq!(r.read(12), 0xFF0);
        assert!(!r.overrun());
        assert_eq!(r.read(8), 0xF0);
        assert!(r.overrun());
    }

    #[test]
    fn verify_signed_and_rice() {
        // 9-bit two's complement: 0x1FF is -1, 0x100 is -256.
        let mut r = BitReader::new(&[0xFF, 0x80, 0x40, 0x00]);
        assert_eq!(r.read_signed(9), -1);
        // Rice, m = 2: "001" = run 2, "01" = lsb 1 -> (2 << 2) + 1 = 9, sign 1 -> -9.
        let mut r = BitReader::new(&[0b0010_1100]);
        assert_eq!(r.read_rice(2), -9);
        // Zero has no sign bit: "1", "00".
        let mut r = BitReader::new(&[0b1001_0000]);
        assert_eq!(r.read_rice(2), 0);
        assert_eq!(r.position(), 3);
    }

    #[test]
    fn verify_raw_frame_is_copied_and_padded() {
        let mut dec = DstDecoder::new(2, 4).unwrap();
        let mut out = [0u8; 8];
        // DSTCoded = 0, one bit skipped, six zero bits: the first byte is 0.
        dec.decode(&[0, 1, 2, 3, 4, 5, 6, 7, 8], &mut out, true).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        // A short frame is completed with the DSD idle pattern.
        dec.decode(&[0, 9, 9, 9], &mut out, true).unwrap();
        assert_eq!(out, [9, 9, 9, 0x69, 0x69, 0x69, 0x69, 0x69]);
        // The stuffing bits must be zero.
        assert!(dec.decode(&[0x04, 1, 2], &mut out, false).is_err());
        assert!(dec.decode(&[], &mut out, false).is_err());
    }

    #[test]
    fn verify_filter_tables() {
        let mut tables = [[0i16; 256]; 16];
        let coefs: Vec<i32> = (1..=10).collect();
        DstDecoder::build_filter(&mut tables, &coefs);
        // Group 0: taps 1..=8. All history bits set: +36; none: -36; bit 0 only:
        // +1 - (2+...+8) = 1 - 35 = -34.
        assert_eq!(tables[0][0xFF], 36);
        assert_eq!(tables[0][0x00], -36);
        assert_eq!(tables[0][0x01], -34);
        // Group 1: taps 9 and 10, and no more.
        assert_eq!(tables[1][0xFF], 19);
        assert_eq!(tables[1][0x01], 9 - 10);
        assert_eq!(tables[1][0x00], -19);
        // Groups past the order are 0, whatever the history.
        assert!(tables[2].iter().all(|&v| v == 0));
        assert!(tables[15].iter().all(|&v| v == 0));
    }

    #[test]
    fn verify_corrupt_frames_are_errors_not_panics() {
        // Every possible first two bytes, then pseudo random data.
        let mut dec = DstDecoder::new(2, 4704).unwrap();
        let mut out = vec![0u8; dec.frame_bytes()];
        let mut state = 0x1234_5678u32;
        for first in 0..=255u8 {
            let mut frame = vec![first | 0x80];
            for _ in 0..300 {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                frame.push((state >> 24) as u8);
            }
            let _ = dec.decode(&frame, &mut out, true);
        }
    }
}
