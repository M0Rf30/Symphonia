//! Spectral (2-tuple) arithmetic decoder for the USAC/MPEG-H FD core.
//!
//! Ported from `libArithCoding/src/ac_arith_coder.cpp`. The tables
//! (`ARI_PK`, `ARI_LSB2`, `ARI_MERGED_HASH_PS`, `ARI_MERGED_HASH_PS_EXT`) are
//! extracted verbatim into `tables.rs`.

use crate::bits::BitReader;
use crate::tables::{
    ARI_LSB2, ARI_MERGED_HASH_PS, ARI_MERGED_HASH_PS_EXT, ARI_PK,
};

const CBITS_NEW: u32 = 16;
const STAT_BITS_NEW: u32 = 14;
const ARI_Q4_NEW: u32 = 0xFFFF;
const VAL_ESC: u32 = 16;

/// Arithmetic coder statistics (the `Tastat` structure).
#[derive(Clone, Copy, Default)]
pub struct ArithStat {
    pub low: u32,
    pub high: u32,
    pub vobf: u32,
}

/// Persistent context state for the spectral arithmetic coder (`CArcoData`).
pub struct ArcoData {
    /// Context of the previous frame (size `(lg_max / 2) + 4`).
    pub c_prev: Vec<u8>,
    /// Number of lines of the previous frame.
    pub number_lines_prev: usize,
}

impl ArcoData {
    pub fn new() -> Self {
        ArcoData {
            c_prev: Vec::new(),
            number_lines_prev: 0,
        }
    }

    /// Decode one window of arithmetic-coded spectral 2-tuples.
    ///
    /// `spectrum` receives the signed magnitude values (small integers). The
    /// caller must be able to hold `lg_max` entries. Returns an error status.
    pub fn decode_arith_data(
        &mut self,
        br: &mut BitReader,
        spectrum: &mut [i32],
        lg: usize,
        lg_max: usize,
        arith_reset_flag: bool,
    ) -> Result<(), ()> {
        if lg_max < lg {
            return Err(());
        }

        // Clear the output spectrum.
        spectrum[..lg_max].fill(0);

        if self.c_prev.len() != (lg_max / 2) + 4 {
            self.c_prev.resize((lg_max / 2) + 4, 0);
        }

        if arith_reset_flag {
            self.c_prev.fill(0);
        } else if lg_max != self.number_lines_prev {
            if self.number_lines_prev == 0 {
                return Err(());
            }
            copy_table_amrwb_arith2(
                &mut self.c_prev,
                self.number_lines_prev / 2,
                lg_max / 2,
            );
        }

        self.number_lines_prev = lg_max;

        if lg > 0 {
            decode2(br, &mut self.c_prev, spectrum, lg / 2, lg_max / 2)?;
        } else {
            let nt = lg_max / 2;
            self.c_prev[2..2 + nt].fill(1);
        }

        Ok(())
    }
}

/// `copyTableAmrwbArith2`: resamples the context table between block sizes.
fn copy_table_amrwb_arith2(tab: &mut [u8], size_in: usize, size_out: usize) {
    // `tab` in C points at `c_prev + 2`, i.e. the context table proper.
    let t = &mut tab[2..];
    let mut k = 2usize;

    if size_in < size_out {
        t[size_out] = t[size_in];
        t[size_out + 1] = t[size_in + 1];
        if size_in < (size_out >> 2) {
            k = 8;
        } else if size_in == (size_out >> 2) {
            k = 4;
        }

        let mut i = size_out as isize - 1;
        let mut j = size_in as isize - 1;

        while i >= 0 {
            let tq = t[j as usize];
            for _ in 0..(k >> 1) {
                t[i as usize] = tq;
                i -= 1;
                t[i as usize] = tq;
                i -= 1;
            }
            j -= 1;
        }
    } else {
        if size_out < (size_in >> 2) {
            k = 8;
        } else if size_out == (size_in >> 2) {
            k = 4;
        }

        let mut i = 0usize;
        let mut j = 0usize;
        while i < size_out {
            let tq = t[j];
            t[i] = tq;
            i += 1;
            j += k;
        }
        t[size_out] = t[size_in];
        t[size_out + 1] = t[size_in + 1];
    }
}

/// `mul_sbc_14bits`.
#[inline]
fn mul_sbc_14bits(r: u32, c: i32) -> u32 {
    ((r * c as u32) >> STAT_BITS_NEW) as u32
}

/// `ari_decode_14bits`: decode one symbol using the cumulative frequency table.
fn ari_decode_14bits(
    br: &mut BitReader,
    s: &mut ArithStat,
    c_freq: &[i32],
    cfl: usize,
) -> u32 {
    let low = s.low;
    let high = s.high;
    let value = s.vobf;

    let range = high - low + 1;
    let c = (((value - low + 1) << STAT_BITS_NEW) - 1) as u32;

    // The C code uses `p = c_freq - 1` and walks `p`, so the search is over
    // `c_freq[0..cfl]`.
    let mut idx: usize = 0;

    if cfl == (VAL_ESC as usize + 1) {
        // 17 symbols
        if (c_freq[1] as u32 * range) > c {
            idx = 1;
            if (c_freq[idx + 8] as u32 * range) > c {
                idx += 8;
            }
            if (c_freq[idx + 4] as u32 * range) > c {
                idx += 4;
            }
            if (c_freq[idx + 2] as u32 * range) > c {
                idx += 2;
            }
            if (c_freq[idx + 1] as u32 * range) > c {
                idx += 1;
            }
        }
    } else if cfl == 4 {
        if (c_freq[2] as u32 * range) > c {
            idx += 2;
        }
        if (c_freq[idx + 1] as u32 * range) > c {
            idx += 1;
        }
    } else if cfl == 2 {
        if (c_freq[1] as u32 * range) > c {
            idx += 1;
        }
    } else if cfl == 27 {
        let p24 = 24usize;
        if (c_freq[16] as u32 * range) > c {
            idx += 16;
        }
        if (c_freq[idx + 8] as u32 * range) > c {
            idx += 8;
        }
        if idx != p24 {
            if (c_freq[idx + 4] as u32 * range) > c {
                idx += 4;
            }
        }
        if (c_freq[idx + 2] as u32 * range) > c {
            idx += 2;
        }
        if idx != (p24 + 2) {
            if (c_freq[idx + 1] as u32 * range) > c {
                idx += 1;
            }
        }
    } else {
        // Fall back to a linear search for robustness.
        while idx + 1 < cfl && (c_freq[idx + 1] as u32 * range) > c {
            idx += 1;
        }
    }

    let symbol = idx as u32;

    if symbol != 0 {
        let hi = low + mul_sbc_14bits(range, c_freq[symbol as usize - 1]);
        s.high = hi.wrapping_sub(1);
    }
    s.low = low + mul_sbc_14bits(range, c_freq[symbol as usize]);

    let mut us_high = (s.high as u16) as u32;
    let mut us_low = (s.low as u16) as u32;
    let mut val = value;

    loop {
        if us_high & 0x8000 != 0 {
            if us_low & 0x8000 == 0 {
                if us_low & 0x4000 != 0 && us_high & 0x4000 == 0 {
                    us_low = us_low.wrapping_sub(0x4000);
                    us_high = us_high.wrapping_sub(0x4000);
                    val = val.wrapping_sub(0x4000);
                } else {
                    break;
                }
            }
        }
        us_low = (us_low << 1) & 0xFFFF;
        us_high = ((us_high << 1) | 1) & 0xFFFF;
        val = ((val << 1) | br.read_bit()) & 0xFFFF;
    }

    s.low = us_low;
    s.high = us_high;
    s.vobf = val & 0xFFFF;

    symbol
}

/// `get_pk_v2`: resolve the cumulative frequency table index for a context.
#[inline]
fn get_pk_v2(s: u32) -> usize {
    if s <= 0xF {
        return ARI_MERGED_HASH_PS_EXT[s as usize] as usize;
    }

    let mut p: usize = 0;
    let s12 = (s << 12) - 1;
    if s12 > ARI_MERGED_HASH_PS[485] {
        p += 486;
    } else if s12 > ARI_MERGED_HASH_PS[255] {
        p += 256;
    }

    if s12 > ARI_MERGED_HASH_PS[p + 127] {
        p += 128;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 63] {
        p += 64;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 31] {
        p += 32;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 15] {
        p += 16;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 7] {
        p += 8;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 3] {
        p += 4;
    }
    if s12 > ARI_MERGED_HASH_PS[p + 1] {
        p += 2;
    }
    let mut j = ARI_MERGED_HASH_PS[p];
    if s12 > j {
        j = ARI_MERGED_HASH_PS[p + 1];
    }
    let jj = if s != (j >> 12) { j >> 6 } else { j };
    (jj & 0x3F) as usize
}

/// `decode2`: decode `n` spectral 2-tuples.
fn decode2(
    br: &mut BitReader,
    c_prev: &mut [u8],
    spectrum: &mut [i32],
    n: usize,
    nt: usize,
) -> Result<(), ()> {
    let mut st = ArithStat {
        low: 0,
        high: ARI_Q4_NEW,
        vobf: br.read_bits(CBITS_NEW),
    };

    let mut c_3: u32 = 0;
    let mut c_2: u32 = 0;
    let mut c_1: u32 = 0;
    let mut c_0: u32 = 1;

    // In C, `decode2` receives `c_prev + 2`; the context table proper starts at
    // offset 2, with two leading zero slots used for `c_prev[0]`/`c_prev[1]`.
    let mut state_inc = (c_prev[2] as u32) << 12;

    let mut i = 0usize;
    while i < n {
        let mut s = state_inc >> 8;
        s += (c_prev[i + 3] as u32) << 8;
        s = (s << 4) + c_1;
        state_inc = s;

        if i > 3 {
            if c_1 + c_2 + c_3 < 5 {
                s += 0x10000;
            }
        }

        // MSB decoding.
        let mut lev: u32 = 0;
        let mut esc_nb: u32 = 0;
        let r = loop {
            let pki = get_pk_v2(s + (esc_nb << (VAL_ESC + 1)));
            let r = ari_decode_14bits(br, &mut st, &ARI_PK[pki], VAL_ESC as usize + 1);
            if r < VAL_ESC {
                break r;
            }
            lev += 1;
            if lev > 23 {
                return Err(());
            }
            if esc_nb < 7 {
                esc_nb += 1;
            }
        };

        if r == 0 {
            if esc_nb > 0 {
                break;
            }
            c_0 = 1;
        } else {
            let mut b = r >> 2;
            let mut a = r & 0x3;

            for _ in 0..lev {
                let pidx = if a == 0 { 1 } else if b == 0 { 0 } else { 2 };
                let rr = ari_decode_14bits(br, &mut st, &ARI_LSB2[pidx], 4);
                a = (a << 1) | (rr & 1);
                b = (b << 1) | (rr >> 1);
            }

            spectrum[2 * i] = a as i32;
            spectrum[2 * i + 1] = b as i32;

            c_0 = a + b + 1;
            if c_0 > 0xF {
                c_0 = 0xF;
            }
        }

        c_3 = c_2;
        c_2 = c_1;
        c_1 = c_0;
        c_prev[i + 2] = c_0 as u8;

        i += 1;
    }

    br.push_back((CBITS_NEW - 2) as usize);

    // Sign bits.
    for j in 0..i {
        let mut bits = 0u32;
        if spectrum[2 * j] != 0 {
            bits += 1;
        }
        if spectrum[2 * j + 1] != 0 {
            bits += 1;
        }
        if bits != 0 {
            let r = br.read_bits(bits);
            if spectrum[2 * j] != 0 && (r >> (bits - 1)) == 0 {
                spectrum[2 * j] = -spectrum[2 * j];
            }
            if spectrum[2 * j + 1] != 0 && (r & 1) == 0 {
                spectrum[2 * j + 1] = -spectrum[2 * j + 1];
            }
        }
    }

    // Memclear the remaining context entries.
    for k in i..nt {
        c_prev[k + 2] = 1;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_pk_small() {
        // Small contexts use the ext table; just ensure no panic.
        for s in 0..16u32 {
            let _ = get_pk_v2(s);
        }
        for s in [0x100u32, 0x1000u32, 0xFFFFu32, 0x2_0000u32] {
            let _ = get_pk_v2(s);
        }
    }
}
