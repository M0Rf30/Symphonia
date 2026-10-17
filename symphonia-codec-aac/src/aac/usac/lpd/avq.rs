// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The algebraic vector quantiser of USAC (ISO/IEC 23003-3 7.14.2): the decoding of a vector of
//! the RE8 lattice, from the base codebooks `Q0`..`Q4` and their Voronoi extensions.
//!
//! The algorithm and the tables are those of libxaac (`ixheaacd_avq_dec.c`, Apache-2.0).

use super::tables::{
    ABSOLUTE_LEADER_TAB, CARDINALITY_OFFSET_I3, CARDINALITY_OFFSET_I4, FACTORIAL_7, ISO_CODE_DATA,
    ISO_CODE_INDEX, ISO_CODE_NUM, POS_ABS_LEADERS_A3, POS_ABS_LEADERS_A4, SIGNED_LEADER_IS,
};

/// The nearest point of the lattice `2 * Z^8` with a coordinate sum of a multiple of 4 (`D8`
/// scaled), updating the residual `rem`.
fn nearest_neighbor(x: &[i32; 8], count: u32, rem: &mut [i32; 8]) -> [i32; 8] {
    let mut y = [0i32; 8];
    let mut sum = 0i32;

    for i in 0..8 {
        y[i] = if x[i] < 0 {
            (1i32.wrapping_sub(x[i]) >> 1).wrapping_shl(1).wrapping_neg()
        }
        else {
            (1i32.wrapping_add(x[i]) >> 1).wrapping_shl(1)
        };
        sum = sum.wrapping_add(y[i]);

        if x[i] % 2 != 0 {
            let one = 1i32.wrapping_shl(count);
            rem[i] = if x[i] < 0 {
                rem[i].wrapping_sub(one).wrapping_neg()
            }
            else {
                rem[i].wrapping_sub(one)
            };
        }
    }

    if sum % 4 != 0 {
        let mut em = 0i32;
        let mut j = 0;

        for i in 0..8 {
            let s = rem[i].wrapping_abs();
            if em < s {
                em = s;
                j = i;
            }
        }

        let two = 2i32.wrapping_shl(count);

        if rem[j] < 0 {
            y[j] = y[j].wrapping_sub(2);
            rem[j] = rem[j].wrapping_add(two);
        }
        else {
            y[j] = y[j].wrapping_add(2);
            rem[j] = rem[j].wrapping_sub(two);
        }
    }

    y
}

/// The Voronoi search: the lattice point nearest to `x`.
fn voronoi_search(x: &[i32; 8], count: u32, rem1: &mut [i32; 8], rem2: &mut [i32; 8]) -> [i32; 8] {
    let y0 = nearest_neighbor(x, count, rem1);

    let mut x1 = [0i32; 8];

    for i in 0..8 {
        if x[i] == 0 {
            if rem2[i] == 0 {
                x1[i] = x[i] - 1;
            }
            else {
                x1[i] = 0;
                rem2[i] = rem2[i].wrapping_sub(1i32.wrapping_shl(count));
            }
        }
        else {
            x1[i] = x[i].wrapping_sub(1);
        }
    }

    let mut y1 = nearest_neighbor(&x1, count, rem2);

    for v in &mut y1 {
        *v = v.wrapping_add(1);
    }

    let mut e0 = 0i64;
    let mut e1 = 0i64;

    for i in 0..8 {
        e0 += i64::from(rem1[i]) * i64::from(rem1[i]);
        e1 += i64::from(rem2[i]) * i64::from(rem2[i]);
    }

    if e0.min(i64::from(i32::MAX)) < e1.min(i64::from(i32::MAX)) { y0 } else { y1 }
}

/// The index decoding of the Voronoi extension.
fn voronoi_idx_dec(kv: &[i32; 8], m: i32, count: u32) -> [i32; 8] {
    let mut y = [kv[7]; 8];
    let mut z = [0i32; 8];
    let mut rem1 = [0i32; 8];

    let mask = m.wrapping_sub(1);

    z[7] = y[7] >> count;
    rem1[7] = y[7] & mask;

    let mut sum = 0i32;

    for i in (1..=6).rev() {
        let tmp = kv[i].wrapping_shl(1);
        sum = sum.wrapping_add(tmp);
        y[i] = y[i].wrapping_add(tmp);
        z[i] = y[i] >> count;
        rem1[i] = y[i] & mask;
    }

    y[0] = y[0].wrapping_add(kv[0].wrapping_mul(4).wrapping_add(sum));
    z[0] = y[0].wrapping_sub(2) >> count;
    rem1[0] = if m != 0 { y[0].wrapping_sub(2) % m } else { y[0].wrapping_sub(2) };

    let mut rem2 = rem1;

    let v = voronoi_search(&z, count, &mut rem1, &mut rem2);

    for i in 0..8 {
        y[i] = y[i].wrapping_sub(m.wrapping_mul(v[i]));
    }

    y
}

/// The permutation of the leader `xs` of the given rank.
fn gosset_rank_of_permutation(rank: i32, xs: &mut [i32; 8]) {
    let mut a = [0i32; 8];
    let mut w = [0i32; 8];

    let mut j = 0;
    w[0] = 1;
    a[0] = xs[0];
    let mut base = 1i32;

    for i in 1..8 {
        if xs[i] != xs[i - 1] {
            j += 1;
            w[j] = 1;
            a[j] = xs[i];
        }
        else {
            w[j] += 1;
            base = base.wrapping_mul(w[j]);
        }
    }

    if w[0] == 8 {
        *xs = [a[0]; 8];
        return;
    }

    let mut target = rank.wrapping_mul(base);
    let mut fac_b = 1i32;

    for i in 0..8 {
        let fac = fac_b.wrapping_mul(FACTORIAL_7[i]);
        let mut j = usize::MAX;

        loop {
            j = j.wrapping_add(1);
            if j >= 8 {
                // Malformed index: stay on the last class.
                j = 7;
                break;
            }
            target = target.wrapping_sub(w[j].wrapping_mul(fac));
            if target < 0 {
                break;
            }
        }

        xs[i] = a[j];

        target = target.wrapping_add(w[j].wrapping_mul(fac));
        fac_b = fac_b.wrapping_mul(w[j].max(1));
        w[j] -= 1;
    }
}

/// The leader (class) of the code book index in a cardinality offset table.
fn abs_leader_index(table: &[u32], code_book_ind: u32, size: usize) -> usize {
    let mut i = 4;

    while i < size {
        if code_book_ind < table[i] {
            break;
        }
        i += 4;
    }

    if i > size {
        i = size;
    }

    if code_book_ind < table[i - 2] {
        i -= 2;
    }
    if code_book_ind < table[i - 1] {
        i -= 1;
    }

    i - 1
}

/// The vector of the base codebook `n` (`Q0`..`Q4`) of the given index.
fn gosset_decode_base_index(n: i32, code_book_ind: u32) -> [i32; 8] {
    let mut ya = [0i32; 8];

    if n < 2 {
        return ya;
    }

    let idx = match n {
        2 | 3 => {
            let i = abs_leader_index(&CARDINALITY_OFFSET_I3, code_book_ind, 9);
            POS_ABS_LEADERS_A3[i] as usize
        }
        _ => {
            let i = abs_leader_index(&CARDINALITY_OFFSET_I4, code_book_ind, 28);
            POS_ABS_LEADERS_A4[i] as usize
        }
    };

    for i in 0..8 {
        ya[i] = i32::from(ABSOLUTE_LEADER_TAB[idx * 8 + i]);
    }

    let t = ISO_CODE_INDEX[idx] as usize;
    let im = ISO_CODE_NUM[idx] as usize;

    let ks = abs_leader_index(&SIGNED_LEADER_IS[t..], code_book_ind, im);

    let mut sign_code = 2 * i32::from(ISO_CODE_DATA[t + ks]);

    for i in (0..8).rev() {
        ya[i] *= 1 - (sign_code & 2);
        sign_code >>= 1;
    }

    let rank = code_book_ind.wrapping_sub(SIGNED_LEADER_IS[t + ks]) as i32;

    gosset_rank_of_permutation(rank, &mut ya);

    ya
}

/// Decode a vector of the RE8 lattice from the codebook number `qn`, the base codebook index
/// and the 8 Voronoi extension indexes `kv`.
pub fn rotated_gosset_mtx_dec(qn: i32, code_book_idx: u32, kv: &[i32; 8]) -> [i32; 8] {
    if qn <= 4 {
        return gosset_decode_base_index(qn, code_book_idx);
    }

    let mut count = 0u32;
    let mut q = qn;

    while q > 4 {
        count += 1;
        q -= 2;
    }

    let m = if count >= 31 { i32::MAX } else { 1i32 << count };

    let mut b = gosset_decode_base_index(q, code_book_idx);
    let c = voronoi_idx_dec(kv, m, count);

    for i in 0..8 {
        b[i] = m.wrapping_mul(b[i]).wrapping_add(c[i]);
    }

    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Q0` is the zero vector and every other base index decodes to a point of `RE8`: integer
    /// coordinates of equal parity whose sum is a multiple of 4.
    #[test]
    fn base_codebooks_are_lattice_points() {
        assert_eq!(rotated_gosset_mtx_dec(0, 0, &[0; 8]), [0; 8]);

        for (qn, count) in [(2, 256u32), (3, 4096), (4, 65536)] {
            let count = if qn == 2 { 256 } else { count };
            for index in (0..count).step_by(7) {
                let v = rotated_gosset_mtx_dec(qn, index, &[0; 8]);
                let parity = v[0] & 1;
                assert!(v.iter().all(|c| c & 1 == parity), "{qn} {index} {v:?}");
                assert_eq!(v.iter().sum::<i32>() % 4, 0, "{qn} {index} {v:?}");
            }
        }
    }
}
