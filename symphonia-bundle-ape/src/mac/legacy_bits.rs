// Ported from the Monkey's Audio SDK 13.27 (Matthew T. Ashland, BSD 3-clause; see NOTICE in this
// directory): MACLib/Old/UnBitArrayOld.cpp (CUnBitArrayOld) and the bit reading primitives of
// MACLib/UnBitArrayBase.cpp.

//! The bit-granular entropy decoders of the files older than version 3.90 (3900): the "old" Rice
//! coder (before 3.86) and the adaptive Rice coder of 3.86 to 3.89.
//!
//! The stream is read as little-endian 32-bit words, most significant bit first. Unlike the range
//! coded versions the arrays of these versions are not byte aligned: the first array of a frame
//! may start at any bit, and the following arrays continue in the same bit stream.

use crate::mac::error::{ApeError, ApeResult};

/// Bits that the stream may be read beyond its end before it is declared corrupt. The SDK
/// decoders read ahead of what they consume, and zero fill the end of the file.
const OVERRUN_SLACK_BITS: u64 = 64;

const fn boundary_table(zero_first: bool, shift: u32, last: usize) -> [u32; 32] {
    let mut table = [0u32; 32];
    let mut i = if zero_first { 1 } else { 0 };
    while i <= last {
        table[i] = 1u32 << (i as u32 + shift);
        i += 1;
    }
    table
}

// K_SUM_MIN_BOUNDARY_OLD / K_SUM_MAX_BOUNDARY_OLD (UnBitArrayOld.cpp)
const K_SUM_MIN_BOUNDARY_OLD: [u32; 32] = boundary_table(true, 6, 25);
const K_SUM_MAX_BOUNDARY_OLD: [u32; 32] = boundary_table(false, 7, 24);

// K_SUM_MIN_BOUNDARY / K_SUM_MAX_BOUNDARY (UnBitArrayBase.cpp)
const K_SUM_MIN_BOUNDARY: [u32; 32] = boundary_table(true, 4, 27);
const K_SUM_MAX_BOUNDARY: [u32; 32] = boundary_table(false, 5, 26);

/// A most-significant-bit-first reader over the little-endian 32-bit words of a frame.
pub struct BitCursor<'a> {
    raw: &'a [u8],
    index: u64,
}

impl<'a> BitCursor<'a> {
    /// Start `start_bit` bits into the stream.
    pub fn new(raw: &'a [u8], start_bit: u64) -> Self {
        BitCursor { raw, index: start_bit }
    }

    /// The word `idx` of the stream, zero padded past the end of the data.
    #[inline(always)]
    fn word(&self, idx: u64) -> u32 {
        let start = (idx as usize).saturating_mul(4);
        let mut bytes = [0u8; 4];
        if let Some(rest) = self.raw.get(start..) {
            let n = rest.len().min(4);
            bytes[..n].copy_from_slice(&rest[..n]);
        }
        u32::from_le_bytes(bytes)
    }

    fn limit(&self) -> u64 {
        (self.raw.len() as u64) * 8 + OVERRUN_SLACK_BITS
    }

    /// Fail if more of the stream was consumed than the data holds.
    pub fn check_overrun(&self) -> ApeResult<()> {
        if self.index > self.limit() {
            Err(ApeError::DecodingError("bit array: read past the end of the frame"))
        }
        else {
            Ok(())
        }
    }

    /// Read `n` (at most 32) bits.
    #[inline(always)]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }

        let idx = self.index >> 5;
        let shift = (self.index & 31) as u32;
        let pair = (u64::from(self.word(idx)) << 32) | u64::from(self.word(idx + 1));
        self.index += u64::from(n);

        ((pair << shift) >> (64 - n)) as u32
    }

    /// The number of zero bits before the next one bit (which is consumed).
    #[inline]
    fn read_unary(&mut self) -> ApeResult<u32> {
        let limit = self.limit();
        let mut count = 0u32;

        loop {
            let idx = self.index >> 5;
            let shift = (self.index & 31) as u32;
            // The remaining bits of the current word, left aligned.
            let bits = self.word(idx) << shift;

            if bits != 0 {
                let zeros = bits.leading_zeros();
                self.index += u64::from(zeros) + 1;
                return Ok(count + zeros);
            }

            let consumed = 32 - u64::from(shift);
            self.index += consumed;
            count = count.wrapping_add(consumed as u32);

            if self.index > limit {
                return Err(ApeError::DecodingError("bit array: unary code runs past the frame"));
            }
        }
    }

    /// `DecodeValueXBits`
    pub fn decode_x_bits(&mut self, n: u32) -> u32 {
        self.read_bits(n)
    }

    /// `DecodeValueRiceUnsigned`: a unary overflow count, followed by `k` bits.
    pub fn decode_rice_unsigned(&mut self, k: u32) -> ApeResult<u32> {
        let overflow = self.read_unary()?;

        if k == 0 {
            Ok(overflow)
        }
        else if k > 32 {
            Err(ApeError::DecodingError("bit array: invalid Rice parameter"))
        }
        else {
            Ok(overflow.wrapping_shl(k) | self.read_bits(k))
        }
    }
}

/// `Get_K`: the number of bits of `x`.
#[inline]
fn get_k(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// The conversion of an interleaved unsigned value to a signed one.
#[inline(always)]
fn to_signed(v: i32) -> i32 {
    if v & 1 != 0 { (v >> 1).wrapping_add(1) } else { (v >> 1).wrapping_neg() }
}

/// `CUnBitArrayOld::GenerateArrayOld` (versions before 3860).
pub fn generate_array_old(bits: &mut BitCursor<'_>, out: &mut [i32]) -> ApeResult<()> {
    let n = out.len();

    let first = n.min(5);
    for v in out[..first].iter_mut() {
        *v = bits.decode_rice_unsigned(10)? as i32;
    }

    if n <= 5 {
        for v in out.iter_mut() {
            *v = to_signed(*v);
        }
        return bits.check_overrun();
    }

    let mut k_sum: u32 = out[..5].iter().fold(0u32, |s, &v| s.wrapping_add(v as u32));
    let mut k = get_k(k_sum / 10);

    #[allow(clippy::needless_range_loop)] // Mirrors the indexing of the SDK.
    for q in 5..n.min(64) {
        let v = bits.decode_rice_unsigned(k)?;
        out[q] = v as i32;
        k_sum = k_sum.wrapping_add(v);
        k = get_k(k_sum / (q as u32 + 1) / 2);
    }

    if n <= 64 {
        for v in out.iter_mut() {
            *v = to_signed(*v);
        }
        return bits.check_overrun();
    }

    k = get_k(k_sum >> 7);
    let mut k_min = K_SUM_MIN_BOUNDARY_OLD[k as usize & 31];
    let mut k_max = K_SUM_MAX_BOUNDARY_OLD[k as usize & 31];

    // The residual `p1` enters the window, the one 64 positions behind (`p2`) leaves it.
    for p1 in 64..n {
        let p2 = p1 - 64;

        let v = bits.decode_rice_unsigned(k)?;
        out[p1] = v as i32;

        k_sum = k_sum.wrapping_add(out[p1].wrapping_sub(out[p2]) as u32);

        // `*p2 % 2` of the SDK: the same as `& 1` for the non-negative values of the window.
        out[p2] = if out[p2] % 2 != 0 {
            (out[p2] >> 1).wrapping_add(1)
        }
        else {
            (out[p2] >> 1).wrapping_neg()
        };

        if k_sum < k_min {
            loop {
                k = k.wrapping_sub(1);
                if k_sum >= K_SUM_MIN_BOUNDARY_OLD[k as usize & 31] {
                    break;
                }
            }

            k_min = K_SUM_MIN_BOUNDARY_OLD[k as usize & 31];
            k_max = K_SUM_MAX_BOUNDARY_OLD[k as usize & 31];
        }
        else if k_sum >= k_max {
            while K_SUM_MAX_BOUNDARY_OLD[(k as usize + 1) & 31] != 0 {
                k += 1;
                if k_sum < K_SUM_MAX_BOUNDARY_OLD[k as usize & 31] {
                    break;
                }
            }

            k_min = K_SUM_MIN_BOUNDARY_OLD[k as usize & 31];
            k_max = K_SUM_MAX_BOUNDARY_OLD[k as usize & 31];
        }
    }

    for v in out[n - 64..].iter_mut() {
        *v = to_signed(*v);
    }

    bits.check_overrun()
}

/// `CUnBitArrayOld::GenerateArrayRice` (versions 3860 to 3890).
pub fn generate_array_rice(
    bits: &mut BitCursor<'_>,
    out: &mut [i32],
    version: i32,
) -> ApeResult<()> {
    let cap_overflow = version > 3880;

    let mut k: u32 = 10;
    let mut k_sum: u32 = 1 << 14;

    for o in out.iter_mut() {
        let mut overflow = bits.read_unary()?;

        if cap_overflow {
            while overflow >= 16 {
                k += 4;
                overflow -= 16;
            }
        }

        let v = if k != 0 {
            if k > 31 {
                return Err(ApeError::DecodingError("bit array: invalid Rice parameter"));
            }
            overflow.wrapping_shl(k) | bits.read_bits(k)
        }
        else {
            overflow
        };

        k_sum = k_sum.wrapping_add(v).wrapping_sub(k_sum.wrapping_add(8) >> 4);

        // `if (k > 31) k = 31` cannot happen after the check above, apart from the increment.
        let ki = k as usize & 31;
        if k_sum < K_SUM_MIN_BOUNDARY[ki] {
            k -= 1;
        }
        else if K_SUM_MAX_BOUNDARY[ki] != 0 && k_sum >= K_SUM_MAX_BOUNDARY[ki] {
            k += 1;
        }

        // `(v & 1) ? (int)(v >> 1) + 1 : -(int)(v >> 1)`: the shift is a logical one.
        let half = (v >> 1) as i32;
        *o = if v & 1 != 0 { half.wrapping_add(1) } else { half.wrapping_neg() };
    }

    bits.check_overrun()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_read_most_significant_first_per_little_endian_word() {
        // The word 0x80000001 is stored as [0x01, 0x00, 0x00, 0x80].
        let data = [0x01, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut bits = BitCursor::new(&data, 0);
        assert_eq!(bits.read_bits(1), 1);
        assert_eq!(bits.read_bits(30), 0);
        assert_eq!(bits.read_bits(5), 0b11111);
        let mut bits = BitCursor::new(&data, 0);
        assert_eq!(bits.read_bits(32), 0x8000_0001);
        assert_eq!(bits.read_bits(32), 0xFFFF_FFFF);
        assert_eq!(bits.read_bits(32), 0);
    }

    #[test]
    fn unary_counts_zeros_across_words() {
        // 40 zero bits, then a one bit.
        let data = [0, 0, 0, 0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80];
        // Word 1 is zero, word 2 is 0x80000000: 32 + 0 zero bits, then the one bit... shifted
        // by 32.
        let mut bits = BitCursor::new(&data, 0);
        assert_eq!(bits.read_unary().unwrap(), 64);
        assert_eq!(bits.read_bits(0), 0);
    }

    #[test]
    fn unary_scan_of_zeros_fails_instead_of_looping() {
        let data = [0u8; 16];
        let mut bits = BitCursor::new(&data, 0);
        assert!(bits.read_unary().is_err());
    }

    #[test]
    fn boundary_tables_match_the_sdk() {
        assert_eq!(K_SUM_MIN_BOUNDARY_OLD[0..3], [0, 128, 256]);
        assert_eq!(K_SUM_MIN_BOUNDARY_OLD[25], 2147483648);
        assert_eq!(K_SUM_MIN_BOUNDARY_OLD[26], 0);
        assert_eq!(K_SUM_MAX_BOUNDARY_OLD[0..2], [128, 256]);
        assert_eq!(K_SUM_MAX_BOUNDARY_OLD[24], 2147483648);
        assert_eq!(K_SUM_MAX_BOUNDARY_OLD[25], 0);
        assert_eq!(K_SUM_MIN_BOUNDARY[0..3], [0, 32, 64]);
        assert_eq!(K_SUM_MIN_BOUNDARY[27], 2147483648);
        assert_eq!(K_SUM_MAX_BOUNDARY[0..2], [32, 64]);
        assert_eq!(K_SUM_MAX_BOUNDARY[26], 2147483648);
        assert_eq!(K_SUM_MAX_BOUNDARY[27], 0);
    }
}
