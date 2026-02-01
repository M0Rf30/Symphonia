// Symphonia Opus Entropy Decoder
// Rewritten from xiph/opus reference (celt/entdec.c)
// Based on RFC 6716 range coder specification

use symphonia_core::errors::{Error, Result};

/// Range decoder constants from RFC 6716 and libopus
const EC_SYM_BITS: u32 = 8;          // Bits per symbol
const EC_CODE_BITS: u32 = 32;        // Bits in range/value
const EC_CODE_EXTRA: u32 = 7;        // Extra precision bits
const EC_CODE_TOP: u32 = 1u32 << (EC_CODE_BITS - 1);  // 2^31
const EC_CODE_BOT: u32 = 1u32 << (EC_CODE_BITS - 9);  // 2^23 - normalization threshold
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1;  // 23

/// Range decoder state
pub struct RangeDecoder<'a> {
    /// Input buffer
    buf: &'a [u8],
    /// Current offset in buffer
    offs: usize,
    /// Range (upper bound - lower bound + 1)
    rng: u32,
    /// Current value being decoded
    val: u32,
    /// Buffered byte for normalization
    rem: u8,
    /// Total bits read
    nbits_total: u32,
    /// Error flag
    error: bool,

    // Raw bits from end of buffer
    end_offs: usize,
    end_window: u32,
    nend_bits: u32,
}

impl<'a> RangeDecoder<'a> {
    /// Initialize range decoder from buffer
    /// Reference: ec_dec_init() in celt/entdec.c
    pub fn new(buf: &'a [u8]) -> Result<Self> {
        if buf.is_empty() {
            return Ok(Self {
                buf,
                offs: 0,
                rng: 1u32 << EC_CODE_EXTRA,
                val: 0,
                rem: 0,
                nbits_total: EC_CODE_BITS + 1 - ((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS,
                error: false,
                end_offs: buf.len(),
                end_window: 0,
                nend_bits: 0,
            });
        }

        let rem = buf[0];
        let offs = 1;

        // Initial value: rng - 1 - (rem >> (EC_SYM_BITS - EC_CODE_EXTRA))
        let val = (1u32 << EC_CODE_EXTRA) - 1 - ((rem >> 1) as u32);

        let mut decoder = Self {
            buf,
            offs,
            rng: 1u32 << EC_CODE_EXTRA,
            val,
            rem,
            nbits_total: EC_CODE_BITS + 1 - ((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS,
            error: false,
            end_offs: buf.len(),
            end_window: 0,
            nend_bits: 0,
        };

        decoder.normalize();
        Ok(decoder)
    }

    /// Read next byte from buffer
    #[inline]
    fn read_byte(&mut self) -> u8 {
        if self.offs < self.buf.len() {
            let byte = self.buf[self.offs];
            self.offs += 1;
            byte
        } else {
            self.error = true;
            0
        }
    }

    /// Normalize range to maintain precision
    /// Reference: ec_dec_normalize() in celt/entdec.c
    #[inline]
    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS;
            self.rng <<= EC_SYM_BITS;

            let sym = self.rem as u32;
            self.rem = self.read_byte();

            // Combine: val = ((val << 8) + (255 - ((sym << 8) | rem) >> 1)) & (EC_CODE_TOP - 1)
            let combined = (sym << EC_SYM_BITS) | (self.rem as u32);
            let shift = combined >> (EC_SYM_BITS - EC_CODE_EXTRA);
            self.val = ((self.val << EC_SYM_BITS) + ((1u32 << EC_SYM_BITS) - 1 - shift)) & (EC_CODE_TOP - 1);
        }
    }

    /// Decode a symbol with given total frequency
    /// Reference: ec_decode() in celt/entdec.c
    pub fn decode(&mut self, ft: u32) -> u32 {
        let scale = self.rng / ft;
        let ret = self.val / scale;
        ft - ret.min(ft) - 1
    }

    /// Decode binary value with given number of bits
    /// Reference: ec_decode_bin() in celt/entdec.c
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        let scale = self.rng >> bits;
        let ret = self.val / scale;
        (1u32 << bits) - ret.min(1u32 << bits) - 1
    }

    /// Update decoder state after decoding symbol
    /// Reference: ec_dec_update() in celt/entdec.c
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let scale = self.rng / ft;
        let s = scale * (ft - fh);

        self.val = self.val.wrapping_sub(s);
        if fl > 0 {
            self.rng = scale * (fh - fl);
        } else {
            self.rng -= s;
        }

        self.normalize();
    }

    /// Decode symbol using ICDF (inverse cumulative distribution function)
    /// Reference: ec_dec_icdf() in celt/entdec.c
    ///
    /// The ICDF table contains cumulative frequencies in DESCENDING order
    /// For a symbol with N outcomes, icdf has N entries: [icdf[0], icdf[1], ..., icdf[N-1]]
    /// where icdf[i] = total - cumulative_freq[i]
    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let total = 1u32 << ftb;
        let scale = self.rng >> ftb;
        let d = self.val;
        let mut s = self.rng;
        let mut t;
        let mut ret = 0;

        // Iterate through ICDF table (descending cumulative frequencies)
        for &icdf_val in icdf {
            t = s;
            s = scale * (icdf_val as u32);
            if d < s {
                break;
            }
            ret += 1;
        }

        self.val = d - s;
        self.rng = t - s;
        self.normalize();

        ret
    }

    /// Decode a single bit with logarithmic probability
    /// Reference: ec_dec_bit_logp() in celt/entdec.c
    pub fn decode_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = d < s;

        if ret {
            self.rng = s;
        } else {
            self.val = d - s;
            self.rng = r - s;
        }

        self.normalize();
        ret
    }

    /// Decode unsigned integer up to ft
    /// Reference: ec_dec_uint() in celt/entdec.c
    pub fn decode_uint(&mut self, ft: u32) -> u32 {
        // Find number of bits needed
        let mut ftb = 0u32;
        let mut ft_tmp = ft;
        while ft_tmp > 0 {
            ftb += 1;
            ft_tmp >>= 1;
        }

        if ftb > 8 {
            let t = (ft >> (ftb - 8)).max(1);
            let ftb2 = ftb - 8;
            let ft2 = t << ftb2;
            let s = self.decode(t);
            let mut ret = s << ftb2;

            if s + 1 < t {
                ret += self.decode((1u32 << ftb2) - 1);
            } else {
                ret += ft - ft2;
            }
            ret
        } else {
            self.decode(ft)
        }
    }

    /// Read raw bits from end of buffer
    /// Reference: ec_dec_bits() in celt/entdec.c
    pub fn decode_bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;

        // Read bits from end of buffer as needed
        while available < bits {
            if self.end_offs <= self.offs {
                self.error = true;
                return 0;
            }

            self.end_offs -= 1;
            window |= (self.buf[self.end_offs] as u32) << available;
            available += 8;
            self.nbits_total += 8;
        }

        let ret = window & ((1u32 << bits) - 1);
        window >>= bits;
        available -= bits;

        self.end_window = window;
        self.nend_bits = available;

        ret
    }

    /// Get number of bits available for reading
    pub fn bits_left(&self) -> u32 {
        let bytes_left = self.end_offs.saturating_sub(self.offs);
        (bytes_left as u32) * 8 + self.nend_bits
    }

    /// Get current bit position (fractional precision)
    pub fn tell_frac(&self) -> u32 {
        self.nbits_total - self.nend_bits
    }
}
