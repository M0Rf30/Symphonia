// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Range coder for Monkey's Audio (version >= 3990).
//!
//! All u32 arithmetic uses wrapping operations to match C++ unsigned overflow semantics.

use crate::mac::bitreader::ByteReader;
use crate::mac::error::{ApeError, ApeResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CODE_BITS: u32 = 32;
const TOP_VALUE: u32 = 1u32 << (CODE_BITS - 1); // 0x80000000
const EXTRA_BITS: u32 = (CODE_BITS - 2) % 8 + 1; // 7
pub const BOTTOM_VALUE: u32 = TOP_VALUE >> 8; // 0x00800000
const RANGE_OVERFLOW_SHIFT: u32 = 16;
const MODEL_ELEMENTS: u32 = 64;
const OVERFLOW_SIGNAL: u32 = 1;
const OVERFLOW_PIVOT_VALUE: u32 = 32768;

// ---------------------------------------------------------------------------
// Probability tables (version >= 3990 only)
// ---------------------------------------------------------------------------

#[allow(clippy::large_const_arrays)]
const RANGE_TOTAL_2: [u32; 65] = [
    0, 19578, 36160, 48417, 56323, 60899, 63265, 64435, 64971, 65232, 65351, 65416, 65447, 65466,
    65476, 65482, 65485, 65488, 65490, 65491, 65492, 65493, 65494, 65495, 65496, 65497, 65498,
    65499, 65500, 65501, 65502, 65503, 65504, 65505, 65506, 65507, 65508, 65509, 65510, 65511,
    65512, 65513, 65514, 65515, 65516, 65517, 65518, 65519, 65520, 65521, 65522, 65523, 65524,
    65525, 65526, 65527, 65528, 65529, 65530, 65531, 65532, 65533, 65534, 65535, 65536,
];

const RANGE_WIDTH_2: [u32; 64] = [
    19578, 16582, 12257, 7906, 4576, 2366, 1170, 536, 261, 119, 65, 31, 19, 10, 6, 3, 3, 2, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

// ---------------------------------------------------------------------------
// Overflow lookup table (built at compile time)
// ---------------------------------------------------------------------------

/// Build the 65536-entry overflow lookup table from RANGE_TOTAL_2.
const fn build_overflow_table() -> [u8; 65536] {
    let mut table = [0u8; 65536];
    let mut overflow: usize = 0;
    let mut z: usize = 0;
    while z < 65536 {
        if z as u32 >= RANGE_TOTAL_2[overflow + 1] {
            overflow += 1;
        }
        table[z] = overflow as u8;
        z += 1;
    }
    table
}

static OVERFLOW_TABLE: [u8; 65536] = build_overflow_table();

// ---------------------------------------------------------------------------
// Entropy state
// ---------------------------------------------------------------------------

const K_SUM_MIN_BOUNDARY: [u32; 32] = [
    0,          // [0]
    32,         // [1]
    64,         // [2]
    128,        // [3]
    256,        // [4]
    512,        // [5]
    1024,       // [6]
    2048,       // [7]
    4096,       // [8]
    8192,       // [9]
    16384,      // [10]  <-- initial k=10, k_sum=16384
    32768,      // [11]
    65536,      // [12]
    131072,     // [13]
    262144,     // [14]
    524288,     // [15]
    1048576,    // [16]
    2097152,    // [17]
    4194304,    // [18]
    8388608,    // [19]
    16777216,   // [20]
    33554432,   // [21]
    67108864,   // [22]
    134217728,  // [23]
    268435456,  // [24]
    536870912,  // [25]
    1073741824, // [26]
    2147483648, // [27]
    0,          // [28]  zero sentinel
    0,          // [29]
    0,          // [30]
    0,          // [31]
];

/// Per-channel entropy decoder state tracking the adaptive k parameter.
pub struct EntropyState {
    k: u32,
    k_sum: u32,
}

impl EntropyState {
    pub fn new() -> Self {
        let mut state = EntropyState { k: 0, k_sum: 0 };
        state.flush();
        state
    }

    /// Reset state at the start of each frame.
    pub fn flush(&mut self) {
        self.k = 10;
        self.k_sum = (1u32 << self.k).wrapping_mul(16); // 1024 * 16 = 16384
    }

    /// Adapt `k_sum` and `k` to the unsigned (interleaved) value that was just decoded.
    #[inline(always)]
    fn update(&mut self, value: i64) {
        // (value + 1) / 2 is the magnitude of the signed result.
        self.k_sum = self
            .k_sum
            .wrapping_add(((value + 1) / 2) as u32)
            .wrapping_sub((self.k_sum.wrapping_add(16)) >> 5);

        // k is at most 27 (the boundary after it is the zero sentinel), the masks only remove the
        // bounds checks.
        let k = (self.k & 31) as usize;
        let decrease = self.k_sum < K_SUM_MIN_BOUNDARY[k];
        let next = K_SUM_MIN_BOUNDARY[(k + 1) & 31];
        let increase = !decrease && next != 0 && self.k_sum >= next;
        self.k = self.k - u32::from(decrease) + u32::from(increase);
    }
}

// ---------------------------------------------------------------------------
// RangeCoder
// ---------------------------------------------------------------------------

pub struct RangeCoder {
    low: u32,
    range: u32,
    buffer: u32,
}

impl RangeCoder {
    /// Initialize the range coder at the start of a frame's entropy coded data.
    ///
    /// Skips the mandatory dummy byte, reads the seed byte, and sets initial range/low values.
    pub fn new(br: &mut ByteReader<'_>) -> Self {
        br.next_byte(); // skip dummy byte
        let buffer = br.next_byte(); // seed byte
        RangeCoder {
            buffer,
            low: buffer >> (8 - EXTRA_BITS), // buffer >> 1
            range: 1u32 << EXTRA_BITS,       // 128
        }
    }

    /// Feed one byte of the stream into the range coder state.
    #[inline(always)]
    fn shift_in(&mut self, br: &mut ByteReader<'_>) {
        self.buffer = self.buffer.wrapping_shl(8) | br.next_byte();
        self.low = self.low.wrapping_shl(8) | ((self.buffer >> 1) & 0xFF);
        self.range = self.range.wrapping_shl(8);
    }

    /// Range normalization loop: feeds stream bytes into the range coder until `range >
    /// BOTTOM_VALUE`. Returns `false` if the range wrapped to zero (the end-of-life sentinel of
    /// a corrupt stream), in which case no further data can be decoded.
    #[inline(always)]
    fn normalize(&mut self, br: &mut ByteReader<'_>) -> bool {
        while self.range <= BOTTOM_VALUE {
            self.shift_in(br);
            if self.range == 0 {
                return false;
            }
        }
        true
    }

    /// Decode a value from a uniform distribution of size `1 << shift`, updating `low` to
    /// `low % range` afterward.
    ///
    /// Returns an error if range becomes zero (corrupt input).
    #[inline(always)]
    fn range_decode_fast_with_update(
        &mut self,
        br: &mut ByteReader<'_>,
        shift: u32,
    ) -> ApeResult<u32> {
        // Normalize with corruption check
        while self.range <= BOTTOM_VALUE {
            if self.range == 0 {
                return Err(ApeError::DecodingError(
                    "range coder: range is zero during normalization",
                ));
            }
            self.shift_in(br);
        }

        self.range >>= shift;

        if self.range == 0 {
            return Err(ApeError::DecodingError("range coder: range is zero after shift"));
        }

        let result = self.low / self.range;
        self.low %= self.range;
        Ok(result)
    }

    /// Decode the overflow (quotient) portion of a value using the model probability tables and
    /// the overflow lookup table.
    ///
    /// `pivot_value` may be mutated to `OVERFLOW_PIVOT_VALUE` if the overflow signaling
    /// mechanism fires.
    #[inline(always)]
    fn decode_overflow(
        &mut self,
        br: &mut ByteReader<'_>,
        pivot_value: &mut u32,
    ) -> ApeResult<u32> {
        loop {
            // Decode from a uniform distribution of size 65536. If the range wraps to zero
            // (end of life) the result is zero.
            let range_total = if self.normalize(br) {
                self.range >>= RANGE_OVERFLOW_SHIFT;
                self.low / self.range
            }
            else {
                0
            };

            if range_total >= 65536 {
                return Err(ApeError::DecodingError(
                    "range coder: overflow range_total out of bounds",
                ));
            }

            // Look up the symbol from the 65536-entry table, and update the range coder state
            // using the model probabilities.
            let mut overflow = u32::from(OVERFLOW_TABLE[range_total as usize]) & 63;
            let total = RANGE_TOTAL_2[overflow as usize];
            let width = RANGE_WIDTH_2[overflow as usize];
            self.low = self.low.wrapping_sub(self.range.wrapping_mul(total));
            self.range = self.range.wrapping_mul(width);

            // Handle a large overflow (symbol 63).
            if overflow == (MODEL_ELEMENTS - 1) {
                // Read two 16-bit halves to form a 32-bit overflow value
                overflow = self.range_decode_fast_with_update(br, 16)?;
                overflow <<= 16;
                overflow |= self.range_decode_fast_with_update(br, 16)?;

                // Detect overflow signaling: decode again with a forced pivot
                if overflow == OVERFLOW_SIGNAL {
                    *pivot_value = OVERFLOW_PIVOT_VALUE;
                    continue;
                }
            }

            return Ok(overflow);
        }
    }

    /// Decode a single sample residual value from the range-coded bitstream.
    ///
    /// Returns the signed residual. The value is decoded from an unsigned interleaved
    /// representation (0, +1, -1, +2, -2, ...) and converted to signed form at the end.
    #[inline(always)]
    pub fn decode_value(
        &mut self,
        es: &mut EntropyState,
        br: &mut ByteReader<'_>,
    ) -> ApeResult<i64> {
        // Compute the pivot value from k_sum
        let mut pivot_value: u32 = (es.k_sum >> 5).max(1);

        // Decode the overflow (quotient)
        let overflow: u32 = self.decode_overflow(br, &mut pivot_value)?;

        // Decode the base (remainder) from the uniform distribution [0, pivot_value)
        let base: u32;

        if pivot_value >= (1 << 16) {
            // Large pivot: split into two smaller range-coded values
            let pivot_value_bits: u32 = 32 - pivot_value.leading_zeros();

            // The pivot is split in a part of at most 16 bits and a power of two, so that the
            // lower portion only needs shifts.
            let shift = pivot_value_bits.saturating_sub(16);
            let pivot_value_a: u32 = (pivot_value >> shift).wrapping_add(1);

            // Decode upper portion
            if !self.normalize(br) {
                return Ok(0); // end-of-life
            }
            self.range /= pivot_value_a;
            let base_a = self.low / self.range;
            self.low %= self.range;

            // Decode lower portion
            if !self.normalize(br) {
                return Ok(0); // end-of-life
            }
            self.range >>= shift;
            let base_b = self.low / self.range;
            self.low %= self.range;

            base = base_a.wrapping_shl(shift).wrapping_add(base_b);
        }
        else {
            // Small pivot: single range-coded value
            if !self.normalize(br) {
                return Ok(0); // end-of-life
            }
            self.range /= pivot_value;
            base = self.low / self.range;
            self.low %= self.range;
        }

        // Combine overflow and base into the unsigned interleaved value
        let value: i64 = (base as i64) + (overflow as i64) * (pivot_value as i64);

        // Update k_sum and k
        es.update(value);

        // Convert from unsigned interleaved to signed
        //   odd  values -> positive: (value >> 1) + 1
        //   even values -> non-positive: -(value >> 1)
        let odd = value & 1;
        let m = odd - 1;
        Ok((((value >> 1) ^ m) - m) + odd)
    }
}
