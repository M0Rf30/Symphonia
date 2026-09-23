// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Range coder (entropy coder).
//!
//! Ported from libopus `celt/entdec.c` / `celt/entcode.c` (decoder, used at runtime) and
//! `celt/entenc.c` (encoder, `#[cfg(test)]` only, used to build round-trip tests mirroring
//! libopus `tests/test_unit_entropy.c`). Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! Rust method names are documented against their libopus C counterparts so the port can be
//! diffed function-by-function against the reference implementation.

/// Number of bits output/consumed at a time. C: `EC_SYM_BITS`.
const EC_SYM_BITS: u32 = 8;
/// Total number of bits in each state register. C: `EC_CODE_BITS`.
const EC_CODE_BITS: u32 = 32;
/// Maximum symbol value. C: `EC_SYM_MAX`.
const EC_SYM_MAX: u32 = (1 << EC_SYM_BITS) - 1;
/// Bits to shift a symbol into the high-order position. C: `EC_CODE_SHIFT`.
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1;
/// Carry bit of the high-order range symbol. C: `EC_CODE_TOP`.
const EC_CODE_TOP: u32 = 1u32 << (EC_CODE_BITS - 1);
/// Low-order bit of the high-order range symbol. C: `EC_CODE_BOT`.
const EC_CODE_BOT: u32 = EC_CODE_TOP >> EC_SYM_BITS;
/// Number of bits available for the last, partial symbol in the code field. C: `EC_CODE_EXTRA`.
const EC_CODE_EXTRA: u32 = (EC_CODE_BITS - 2) % EC_SYM_BITS + 1;
/// Number of bits used for the range-coded part of unsigned integers. C: `EC_UINT_BITS`.
const EC_UINT_BITS: u32 = 8;
/// Fractional-bit resolution, i.e. `1 << BITRES` == 8 => 1/8th bit. C: `BITRES`.
pub const BITRES: u32 = 3;
/// Size in bits of the raw-bit window register. C: `EC_WINDOW_SIZE`.
const EC_WINDOW_SIZE: u32 = 32;

#[inline]
fn ilog(v: u32) -> i32 {
    // C: `EC_ILOG`, i.e. `1 + floor(log2(v))`, undefined (here: 0) for `v == 0`.
    32 - v.leading_zeros() as i32
}

/// A range decoder. Ported from libopus `ec_dec` (a `ec_ctx` alias). C: `ec_dec_init` et al.
pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: u32,
    end_offs: u32,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    offs: u32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
}

impl<'a> RangeDecoder<'a> {
    /// C: `ec_dec_init`.
    pub fn new(buf: &'a [u8]) -> Self {
        let mut dec = RangeDecoder {
            buf,
            storage: buf.len() as u32,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: (EC_CODE_BITS + 1) as i32
                - (((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS) as i32,
            offs: 0,
            rng: 1u32 << EC_CODE_EXTRA,
            val: 0,
            ext: 0,
            rem: 0,
            error: false,
        };
        dec.rem = dec.read_byte() as i32;
        dec.val = dec.rng - 1 - ((dec.rem as u32) >> (EC_SYM_BITS - EC_CODE_EXTRA));
        dec.normalize();
        dec
    }

    #[inline]
    fn read_byte(&mut self) -> u32 {
        // C: `ec_read_byte`.
        if self.offs < self.storage {
            let b = self.buf[self.offs as usize] as u32;
            self.offs += 1;
            b
        }
        else {
            0
        }
    }

    #[inline]
    fn read_byte_from_end(&mut self) -> u32 {
        // C: `ec_read_byte_from_end`.
        if self.end_offs < self.storage {
            self.end_offs += 1;
            self.buf[(self.storage - self.end_offs) as usize] as u32
        }
        else {
            0
        }
    }

    #[inline]
    fn normalize(&mut self) {
        // C: `ec_dec_normalize`.
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS as i32;
            self.rng <<= EC_SYM_BITS;
            let mut sym = self.rem as u32;
            self.rem = self.read_byte() as i32;
            sym = (sym << EC_SYM_BITS | self.rem as u32) >> (EC_SYM_BITS - EC_CODE_EXTRA);
            self.val = ((self.val << EC_SYM_BITS) + (EC_SYM_MAX & !sym)) & (EC_CODE_TOP - 1);
        }
    }

    /// C: `ec_decode`.
    pub fn decode(&mut self, ft: u32) -> u32 {
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        ft - (s + 1).min(ft)
    }

    /// C: `ec_decode_bin`.
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        self.ext = self.rng >> bits;
        let s = self.val / self.ext;
        (1u32 << bits) - (s + 1).min(1u32 << bits)
    }

    /// C: `ec_dec_update`.
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext.wrapping_mul(ft - fh);
        self.val -= s;
        self.rng = if fl > 0 { self.ext.wrapping_mul(fh - fl) } else { self.rng - s };
        self.normalize();
    }

    /// C: `ec_dec_bit_logp`. Probability of a "one" is `1/(1<<logp)`.
    pub fn dec_bit_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = d < s;
        if !ret {
            self.val = d - s;
        }
        self.rng = if ret { s } else { r - s };
        self.normalize();
        ret
    }

    /// C: `ec_dec_icdf`. `icdf` is a decreasing-to-zero inverse CDF table.
    pub fn dec_icdf(&mut self, icdf: &[u8], ftb: u32) -> i32 {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret: i32 = -1;
        let mut t;
        loop {
            t = s;
            ret += 1;
            s = r.wrapping_mul(icdf[ret as usize] as u32);
            if d >= s {
                break;
            }
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret
    }

    /// C: `ec_dec_icdf16`.
    pub fn dec_icdf16(&mut self, icdf: &[u16], ftb: u32) -> i32 {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret: i32 = -1;
        let mut t;
        loop {
            t = s;
            ret += 1;
            s = r.wrapping_mul(icdf[ret as usize] as u32);
            if d >= s {
                break;
            }
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret
    }

    /// C: `ec_dec_uint`. `ft` must be `> 1`.
    pub fn dec_uint(&mut self, ft: u32) -> u32 {
        assert!(ft > 1);
        let ft = ft - 1;
        let ftb = ilog(ft);
        if ftb > EC_UINT_BITS as i32 {
            let ftb = ftb - EC_UINT_BITS as i32;
            let ft_s = (ft >> ftb) + 1;
            let s = self.decode(ft_s);
            self.update(s, s + 1, ft_s);
            let t = (s << ftb) | self.dec_bits(ftb as u32);
            if t <= ft {
                t
            }
            else {
                self.error = true;
                ft
            }
        }
        else {
            let ft = ft + 1;
            let s = self.decode(ft);
            self.update(s, s + 1, ft);
            s
        }
    }

    /// C: `ec_dec_bits`. Reads raw (non-range-coded) bits from the back of the buffer.
    pub fn dec_bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if (available as u32) < bits {
            loop {
                window |= self.read_byte_from_end() << available;
                available += EC_SYM_BITS as i32;
                if available > (EC_WINDOW_SIZE - EC_SYM_BITS) as i32 {
                    break;
                }
            }
        }
        let ret = window & ((1u32 << bits) - 1);
        window >>= bits;
        available -= bits as i32;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;
        ret
    }

    /// C: `ec_tell`. Number of bits "used" so far (always a slight over-estimate).
    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng)
    }

    /// C: `ec_tell_frac`. As [`Self::tell`] but scaled by `2**BITRES`.
    pub fn tell_frac(&self) -> u32 {
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = (self.nbits_total as u32) << BITRES;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12).wrapping_sub(8);
        b += (r > CORRECTION[b as usize]) as u32;
        let l = ((l as u32) << 3) + b;
        nbits - l
    }

    /// C: `ec_get_error`.
    pub fn error(&self) -> bool {
        self.error
    }

    /// The final range, used for RFC 8251 packet loss concealment cross-checks.
    /// C: consumers typically read `dec.rng` directly (`opus_decode_native`'s `range_final`).
    pub fn range(&self) -> u32 {
        self.rng
    }
}

#[cfg(test)]
/// A test-only range ENCODER, ported from libopus `celt/entenc.c`. Used exclusively by unit
/// tests to build inputs for [`RangeDecoder`] and to cross-check `tell`/`tell_frac`.
pub(crate) mod test_encoder {
    use super::{EC_CODE_BOT, EC_CODE_SHIFT, EC_CODE_TOP, EC_SYM_BITS, EC_SYM_MAX, EC_UINT_BITS, ilog};

    pub struct RangeEncoder {
        buf: Vec<u8>,
        end_bytes: Vec<u8>,
        end_window: u32,
        nend_bits: i32,
        nbits_total: i32,
        rng: u32,
        val: u32,
        ext: u32,
        rem: i32,
        error: bool,
    }

    impl RangeEncoder {
        pub fn new() -> Self {
            RangeEncoder {
                buf: Vec::new(),
                end_bytes: Vec::new(),
                end_window: 0,
                nend_bits: 0,
                nbits_total: (super::EC_CODE_BITS + 1) as i32,
                rng: EC_CODE_TOP,
                val: 0,
                ext: 0,
                rem: -1,
                error: false,
            }
        }

        fn write_byte(&mut self, v: u32) {
            self.buf.push(v as u8);
        }

        fn write_byte_at_end(&mut self, v: u32) {
            self.end_bytes.push(v as u8);
        }

        fn carry_out(&mut self, c: i32) {
            if c as u32 != EC_SYM_MAX {
                let carry = c >> EC_SYM_BITS;
                if self.rem >= 0 {
                    self.write_byte((self.rem + carry) as u32);
                }
                if self.ext > 0 {
                    let sym = ((EC_SYM_MAX as i32 + carry) & EC_SYM_MAX as i32) as u32;
                    while self.ext > 0 {
                        self.write_byte(sym);
                        self.ext -= 1;
                    }
                }
                self.rem = c & EC_SYM_MAX as i32;
            }
            else {
                self.ext += 1;
            }
        }

        fn normalize(&mut self) {
            while self.rng <= EC_CODE_BOT {
                let c = (self.val >> EC_CODE_SHIFT) as i32;
                self.carry_out(c);
                self.val = (self.val << EC_SYM_BITS) & (EC_CODE_TOP - 1);
                self.rng <<= EC_SYM_BITS;
                self.nbits_total += EC_SYM_BITS as i32;
            }
        }

        pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
            let r = self.rng / ft;
            if fl > 0 {
                self.val = self.val.wrapping_add(self.rng - r.wrapping_mul(ft - fl));
                self.rng = r.wrapping_mul(fh - fl);
            }
            else {
                self.rng -= r.wrapping_mul(ft - fh);
            }
            self.normalize();
        }

        pub fn encode_bin(&mut self, fl: u32, fh: u32, bits: u32) {
            let r = self.rng >> bits;
            if fl > 0 {
                self.val = self.val.wrapping_add(self.rng - r.wrapping_mul((1u32 << bits) - fl));
                self.rng = r.wrapping_mul(fh - fl);
            }
            else {
                self.rng -= r.wrapping_mul((1u32 << bits) - fh);
            }
            self.normalize();
        }

        pub fn enc_bit_logp(&mut self, val: bool, logp: u32) {
            let r = self.rng;
            let l = self.val;
            let s = r >> logp;
            let r = r - s;
            if val {
                self.val = l + r;
            }
            self.rng = if val { s } else { r };
            self.normalize();
        }

        pub fn enc_icdf(&mut self, s: i32, icdf: &[u8], ftb: u32) {
            let r = self.rng >> ftb;
            if s > 0 {
                self.val = self
                    .val
                    .wrapping_add(self.rng - r.wrapping_mul(icdf[(s - 1) as usize] as u32));
                self.rng = r.wrapping_mul(icdf[(s - 1) as usize] as u32 - icdf[s as usize] as u32);
            }
            else {
                self.rng -= r.wrapping_mul(icdf[s as usize] as u32);
            }
            self.normalize();
        }

        pub fn enc_uint(&mut self, fl: u32, ft: u32) {
            assert!(ft > 1);
            let ft = ft - 1;
            let ftb = ilog(ft);
            if ftb > EC_UINT_BITS as i32 {
                let ftb = ftb - EC_UINT_BITS as i32;
                let ft_s = (ft >> ftb) + 1;
                let fl_s = fl >> ftb;
                self.encode(fl_s, fl_s + 1, ft_s);
                self.enc_bits(fl & ((1u32 << ftb) - 1), ftb as u32);
            }
            else {
                self.encode(fl, fl + 1, ft + 1);
            }
        }

        pub fn enc_bits(&mut self, fl: u32, bits: u32) {
            let mut window = self.end_window;
            let mut used = self.nend_bits;
            assert!(bits > 0);
            if used + bits as i32 > 32 {
                loop {
                    self.write_byte_at_end(window & EC_SYM_MAX);
                    window >>= EC_SYM_BITS;
                    used -= EC_SYM_BITS as i32;
                    if used < EC_SYM_BITS as i32 {
                        break;
                    }
                }
            }
            window |= fl << used;
            used += bits as i32;
            self.end_window = window;
            self.nend_bits = used;
            self.nbits_total += bits as i32;
        }

        pub fn tell(&self) -> i32 {
            self.nbits_total - ilog(self.rng)
        }

        pub fn tell_frac(&self) -> u32 {
            const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
            let nbits = (self.nbits_total as u32) << super::BITRES;
            let l = ilog(self.rng);
            let r = self.rng >> (l - 16);
            let mut b = (r >> 12).wrapping_sub(8);
            b += (r > CORRECTION[b as usize]) as u32;
            let l = ((l as u32) << 3) + b;
            nbits - l
        }

        /// C: `ec_enc_done`. Finalizes the stream and returns the packed bytes.
        pub fn done(mut self) -> Vec<u8> {
            let mut l = super::EC_CODE_BITS as i32 - ilog(self.rng);
            let mut msk = (EC_CODE_TOP - 1) >> l;
            let mut end = (self.val.wrapping_add(msk)) & !msk;
            if (end | msk) >= self.val.wrapping_add(self.rng) {
                l += 1;
                msk >>= 1;
                end = (self.val.wrapping_add(msk)) & !msk;
            }
            while l > 0 {
                let c = (end >> EC_CODE_SHIFT) as i32;
                self.carry_out(c);
                end = (end << EC_SYM_BITS) & (EC_CODE_TOP - 1);
                l -= EC_SYM_BITS as i32;
            }
            if self.rem >= 0 || self.ext > 0 {
                self.carry_out(0);
            }
            let mut window = self.end_window;
            let mut used = self.nend_bits;
            while used >= EC_SYM_BITS as i32 {
                self.write_byte_at_end(window & EC_SYM_MAX);
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
            }
            if used > 0 {
                self.write_byte_at_end(window & EC_SYM_MAX);
            }
            // Assemble: range-coded bytes followed by zero padding, then the raw-bit bytes
            // (which were accumulated in forward order but stored from the end of the buffer).
            let mut out = self.buf;
            self.end_bytes.reverse();
            out.extend(self.end_bytes);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_encoder::RangeEncoder;
    use super::RangeDecoder;
    use rand::Rng;
    use rand::SeedableRng;

    #[test]
    fn icdf_round_trip_fixed_seed() {
        // Mirrors libopus tests/test_unit_entropy.c: encode a long sequence of symbols drawn
        // from a small ICDF table, then decode and verify exact symbol recovery.
        let icdf: [u8; 4] = [200, 100, 40, 0];
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
        let symbols: Vec<i32> = (0..20000).map(|_| rng.random_range(0..3)).collect();

        let mut enc = RangeEncoder::new();
        for &s in &symbols {
            enc.enc_icdf(s, &icdf, 8);
        }
        let bytes = enc.done();

        let mut dec = RangeDecoder::new(&bytes);
        for &s in &symbols {
            let d = dec.dec_icdf(&icdf, 8);
            assert_eq!(d, s);
        }
        assert!(!dec.error());
    }

    #[test]
    fn uint_and_bits_round_trip() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let values: Vec<(u32, u32)> =
            (0..5000).map(|_| (rng.random_range(2u32..=1_000_000), 0)).collect();
        let draws: Vec<u32> = values.iter().map(|&(ft, _)| rng.random_range(0..ft)).collect();

        let mut enc = RangeEncoder::new();
        for (&(ft, _), &v) in values.iter().zip(draws.iter()) {
            enc.enc_uint(v, ft);
        }
        // Interleave some raw bits, as opus_decoder.c does for e.g. the extension bit.
        enc.enc_bits(0b1011, 4);
        let bytes = enc.done();

        let mut dec = RangeDecoder::new(&bytes);
        for (&(ft, _), &v) in values.iter().zip(draws.iter()) {
            assert_eq!(dec.dec_uint(ft), v);
        }
        assert_eq!(dec.dec_bits(4), 0b1011);
        assert!(!dec.error());
    }

    #[test]
    fn bit_logp_round_trip() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let bits: Vec<bool> = (0..10000).map(|_| rng.random_bool(0.3)).collect();

        let mut enc = RangeEncoder::new();
        for &b in &bits {
            enc.enc_bit_logp(b, 3);
        }
        let bytes = enc.done();

        let mut dec = RangeDecoder::new(&bytes);
        for &b in &bits {
            assert_eq!(dec.dec_bit_logp(3), b);
        }
    }

    #[test]
    fn tell_matches_between_encoder_and_decoder() {
        let icdf: [u8; 4] = [200, 100, 40, 0];
        let mut enc = RangeEncoder::new();
        let mut telltale_enc = Vec::new();
        for s in [0, 1, 2, 0, 3, 1, 2, 2, 0, 1] {
            enc.enc_icdf(s, &icdf, 8);
            telltale_enc.push((enc.tell(), enc.tell_frac()));
        }
        let bytes = enc.done();

        let mut dec = RangeDecoder::new(&bytes);
        for (i, s) in [0, 1, 2, 0, 3, 1, 2, 2, 0, 1].into_iter().enumerate() {
            let d = dec.dec_icdf(&icdf, 8);
            assert_eq!(d, s);
            // ec_tell()/ec_tell_frac() are monotonic non-decreasing and always a slight
            // over-estimate; the decoder's running tally trails or matches the encoder's.
            let (et, etf) = telltale_enc[i];
            assert!(dec.tell() <= et + 1);
            assert!(dec.tell_frac() <= etf + 8);
        }
    }

    #[test]
    fn empty_and_short_buffers_do_not_panic() {
        let mut dec = RangeDecoder::new(&[]);
        let _ = dec.dec_icdf(&[200, 100, 40, 0], 8);
        let mut dec = RangeDecoder::new(&[0xFF]);
        let _ = dec.dec_uint(5);
    }
}
