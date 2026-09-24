// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Insertion sort. Ported from libopus `silk/sort.c` (BSD-3-Clause), see NOTICE. Only
//! `silk_insertion_sort_increasing_all_values_int16` is used by the decoder (the NLSF
//! stabilizer's safe fallback path); the encoder-only sort variants are omitted.

#![allow(dead_code)]

/// C: `silk_insertion_sort_increasing_all_values_int16`. Insertion sort (fast for
/// already-almost-sorted arrays), in place, ascending.
pub(crate) fn silk_insertion_sort_increasing_all_values_int16(a: &mut [i16]) {
    let l = a.len();
    for i in 1..l {
        let value = a[i];
        let mut j = i;
        while j > 0 && value < a[j - 1] {
            a[j] = a[j - 1];
            j -= 1;
        }
        a[j] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_ascending() {
        let mut a = [5i16, 3, -1, 42, 0, 0, 7];
        silk_insertion_sort_increasing_all_values_int16(&mut a);
        assert_eq!(a, [-1, 0, 0, 3, 5, 7, 42]);
    }

    #[test]
    fn already_sorted_is_noop() {
        let mut a = [1i16, 2, 3, 4];
        silk_insertion_sort_increasing_all_values_int16(&mut a);
        assert_eq!(a, [1, 2, 3, 4]);
    }
}
