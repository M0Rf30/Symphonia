// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The mp3PRO SBR bitstream parser (spec sections 3 to 9 and 11.6).
//!
//! [`SbrParser`] consumes the ancillary data of one MPEG-2/2.5 Layer III frame and produces, per
//! channel, the dequantised parameters of an SBR frame ([`SbrFrame`]):
//!
//! 1. The block framing and the 7-bit CRC (sections 3 and 4) and the optional SBR header
//!    (section 5).
//! 2. The channel pair or single channel element (section 6) with its time/frequency grid
//!    (section 7), direction flags, inverse filtering modes, and Huffman coded envelope and
//!    noise-floor deltas (section 9).
//! 3. The decoding of the deltas into absolute exponents with the previous-frame memory, error
//!    concealment and level/balance coupling, and their conversion to linear energies
//!    (section 11.6).
//!
//! The decoder state (which channels are in the "SBR active" state, the memory of the previous
//! frame, and whether SBR was disabled for good) follows the reference decoder exactly, including
//! its quirks. The state is therefore kept in the same shape as the reference's per-channel frame
//! data: a header word array, the frame info, the raw and decoded envelope values, and the
//! previous-frame data.

use super::freq;
use super::huffman::{self, BitReader};
use super::{
    FreqTables, MAX_BANDS_HIGH, MAX_ENVELOPES, MAX_NOISE_BANDS, MAX_NOISE_ENVELOPES, ParseStatus,
    SbrFrame, SbrHeader,
};

/// The number of time slots in a frame.
const TIME_SLOTS: i32 = 9;

/// The number of 32-bit words of the header as held by the decoder.
const HEADER_WORDS: usize = 22;

/// The maximum number of envelope values in a frame.
const MAX_ENV_VALUES: usize = MAX_ENVELOPES * MAX_BANDS_HIGH;

/// The maximum number of noise-floor values in a frame.
const MAX_NOISE_VALUES: usize = MAX_NOISE_ENVELOPES * MAX_NOISE_BANDS;

/// The `sbr_dec` return codes of the reference decoder that switch SBR off.
const ERR_PROTOCOL: u32 = 0x0D;
const ERR_RESET: u32 = 0x0C;

/// The coupling states of a channel.
const COUPLING_NONE: u8 = 0;
const COUPLING_LEVEL: u8 = 1;
const COUPLING_BALANCE: u8 = 2;

/// Indices into the header words.
mod hw {
    /// 3 until the first header has been accepted, then 0 (unchanged), 1 (changed) or 2 (protocol
    /// mismatch).
    pub const STATUS: usize = 0;
    /// Set if start, stop, or the extra scale fields changed. Informational only.
    pub const FREQ_CHANGED: usize = 1;
    /// The frame data error flag (shared with the header words in the reference decoder).
    pub const ERROR: usize = 2;
    pub const PROTOCOL: usize = 3;
    /// The "must equal the previous frame" bit of the element.
    pub const ELEMENT_BIT: usize = 5;
    /// 2 for a channel pair stream.
    pub const ELEMENT_KIND: usize = 6;
    pub const AMP_RES: usize = 7;
    pub const START: usize = 8;
    pub const STOP: usize = 9;
    pub const XOVER: usize = 10;
    pub const FREQ_SCALE: usize = 11;
    pub const ALTER_SCALE: usize = 12;
    pub const NOISE_BANDS: usize = 13;
    pub const NQ: usize = 14;
    pub const DATA_EXTRA: usize = 15;
    pub const LIMITER_BANDS: usize = 16;
    pub const LIMITER_GAINS: usize = 17;
    pub const INTERPOL_FREQ: usize = 18;
    pub const SMOOTHING_MODE: usize = 19;
    pub const LC_STEREO_MODE: usize = 20;
    pub const LC_STEREO_ACTIVE: usize = 21;
}

/// The header of a decoder that has not seen a header yet.
const DEFAULT_HEADER: [i32; HEADER_WORDS] =
    [3, 1, 0, 0, 1, 1, 0, 1, 5, 0, 0, 2, 1, 2, 0, 0, 2, 2, 1, 1, 2, 0];

/// The time/frequency grid of a channel: the "frame info" of the reference decoder.
#[derive(Copy, Clone)]
struct FrameInfo {
    num_env: u8,
    /// Envelope borders in time slots.
    borders: [u8; MAX_ENVELOPES + 1],
    /// Frequency resolution (non-zero = high) per envelope.
    freq_res: [u8; MAX_ENVELOPES],
    /// The transient envelope, or -1.
    transient: i32,
    num_noise: u8,
    noise_borders: [u8; MAX_NOISE_ENVELOPES + 1],
}

impl FrameInfo {
    /// The FIXFIX grids with 1, 2 and 4 envelopes, all in high frequency resolution.
    fn fixfix(num_env: u8) -> Self {
        match num_env {
            1 => FrameInfo {
                num_env: 1,
                borders: [0, 9, 0, 0, 0],
                freq_res: [1, 0, 0, 0],
                transient: -1,
                num_noise: 1,
                noise_borders: [0, 9, 0],
            },
            2 => FrameInfo {
                num_env: 2,
                borders: [0, 5, 9, 0, 0],
                freq_res: [1, 1, 0, 0],
                transient: -1,
                num_noise: 2,
                noise_borders: [0, 5, 9],
            },
            _ => FrameInfo {
                num_env: 4,
                borders: [0, 2, 5, 7, 9],
                freq_res: [1, 1, 1, 1],
                transient: -1,
                num_noise: 2,
                noise_borders: [0, 5, 9],
            },
        }
    }
}

/// The memory of the previous frame of a channel.
#[derive(Copy, Clone)]
struct Previous {
    /// The envelope exponents per high-resolution band.
    env: [f32; MAX_BANDS_HIGH],
    /// The noise-floor exponents of the last noise envelope.
    noise: [f32; MAX_NOISE_BANDS],
    coupling: u8,
    /// The previous frame was concealed, or its first envelope needs a border correction.
    flag: bool,
}

/// All per-channel state of the parser.
#[derive(Copy, Clone)]
struct Chan {
    /// The number of envelope values of the frame.
    total: usize,
    n_low: usize,
    n_high: usize,
    nq: usize,
    /// The element carries an inverse filtering field (false until the first header reset).
    has_invf: bool,
    /// The offset of the low-resolution to high-resolution band map. This is set once at
    /// creation (`2 * n_low - n_high` of the default tables) and never updated.
    v4: usize,
    hw: [i32; HEADER_WORDS],
    fi: FrameInfo,
    dt_env: [u8; MAX_ENVELOPES],
    dt_noise: [u8; MAX_NOISE_ENVELOPES],
    /// The last border of the previous frame.
    prev_end: i32,
    /// The inverse filtering mode of this frame (0) and of the previous frame (1).
    invf: [u32; 2],
    data_extra: u32,
    coupling: u8,
    /// Raw deltas, then exponents, then linear energies.
    env: [f32; MAX_ENV_VALUES],
    /// Raw deltas, then exponents, then linear noise-to-signal ratios.
    noise: [f32; MAX_NOISE_VALUES],
    prev: Previous,
}

impl Chan {
    fn new(hw: [i32; HEADER_WORDS], tables: &FreqTables) -> Self {
        Chan {
            total: 0,
            n_low: tables.n_low,
            n_high: tables.n_high,
            nq: 1,
            has_invf: false,
            v4: (2 * tables.n_low).saturating_sub(tables.n_high),
            hw,
            fi: FrameInfo::fixfix(1),
            dt_env: [0; MAX_ENVELOPES],
            dt_noise: [0; MAX_NOISE_ENVELOPES],
            prev_end: TIME_SLOTS,
            invf: [0; 2],
            data_extra: 0,
            coupling: COUPLING_NONE,
            env: [0.0; MAX_ENV_VALUES],
            noise: [0.0; MAX_NOISE_VALUES],
            prev: Previous {
                env: [0.0; MAX_BANDS_HIGH],
                noise: [0.0; MAX_NOISE_BANDS],
                coupling: 0,
                flag: false,
            },
        }
    }

    fn error(&self) -> bool {
        self.hw[hw::ERROR] != 0
    }

    fn amp_res(&self) -> i32 {
        self.hw[hw::AMP_RES]
    }

    /// The number of bands of the frequency table of envelope `e`.
    fn bands(&self, e: usize) -> usize {
        if self.fi.freq_res[e] != 0 { self.n_high } else { self.n_low }
    }
}

/// The header fields of the header words.
fn header_of(hw: &[i32; HEADER_WORDS]) -> SbrHeader {
    SbrHeader {
        amp_res: hw[hw::AMP_RES] as u8,
        start_freq: hw[hw::START] as u8,
        stop_freq: hw[hw::STOP] as u8,
        xover_band: hw[hw::XOVER] as u8,
        freq_scale: hw[hw::FREQ_SCALE] as u8,
        alter_scale: hw[hw::ALTER_SCALE] as u8,
        noise_bands: hw[hw::NOISE_BANDS] as u8,
        limiter_bands: hw[hw::LIMITER_BANDS] as u8,
        limiter_gains: hw[hw::LIMITER_GAINS] as u8,
        interpol_freq: hw[hw::INTERPOL_FREQ] as u8,
        smoothing_mode: hw[hw::SMOOTHING_MODE] as u8,
    }
}

/// The CRC of the block: a bitwise 7-bit CRC (polynomial `x^7 + x^6 + x^2 + 1`, seed all ones)
/// over `n` bits of `data` starting at bit `pos` (spec section 4).
fn crc7(data: &[u8], pos: usize, n: usize) -> u32 {
    let mut reg = 0xffffu32;
    for i in pos..pos + n {
        let bit = u32::from((data[i >> 3] >> (7 - (i & 7))) & 1);
        let mut next = (reg << 1) & 0xffff;
        if bit != u32::from(reg & 0x80 != 0) {
            next ^= 0x45;
        }
        reg = next;
    }
    reg & 0x7f
}

/// Parses the SBR header (spec section 5) given the header words `old` of the previous frame,
/// and returns the new header words and the comparison result: 0 = unchanged, 1 = changed
/// (reset required), 2 = protocol mismatch (old header kept), 3 = protocol mismatch before any
/// header was accepted.
fn parse_header(
    r: &mut BitReader<'_>,
    old: &[i32; HEADER_WORDS],
    n_channels: usize,
    core_rate: u32,
) -> ([i32; HEADER_WORDS], u32) {
    let mut h = *old;

    h[hw::PROTOCOL] = r.get(2) as i32;
    h[hw::AMP_RES] = r.get(1) as i32;
    h[hw::START] = r.get(4) as i32;
    h[hw::STOP] = r.get(4) as i32;
    h[hw::XOVER] = r.get(3) as i32;
    r.skip(2);
    h[hw::DATA_EXTRA] = r.get(1) as i32;
    let extra_1 = r.get(1) != 0;
    let extra_2 = r.get(1) != 0;

    if n_channels == 2 {
        h[hw::ELEMENT_KIND] = 2;
    }
    else {
        if h[hw::ELEMENT_KIND] == 2 {
            h[hw::ELEMENT_KIND] = 3;
        }
        if h[hw::LC_STEREO_ACTIVE] != 0 {
            r.skip(2);
        }
        else {
            h[hw::LC_STEREO_MODE] = r.get(2) as i32;
        }
    }

    if extra_1 {
        h[hw::FREQ_SCALE] = r.get(2) as i32;
        h[hw::ALTER_SCALE] = r.get(1) as i32;
        h[hw::NOISE_BANDS] = r.get(2) as i32;
    }
    else {
        h[hw::FREQ_SCALE] = 2;
        h[hw::ALTER_SCALE] = 1;
        h[hw::NOISE_BANDS] = 2;
    }

    if extra_2 {
        h[hw::LIMITER_BANDS] = r.get(2) as i32;
        h[hw::LIMITER_GAINS] = r.get(2) as i32;
        h[hw::INTERPOL_FREQ] = r.get(1) as i32;
        h[hw::SMOOTHING_MODE] = r.get(1) as i32;
        r.skip(1);
    }
    else {
        h[hw::LIMITER_BANDS] = 2;
        h[hw::LIMITER_GAINS] = 2;
        h[hw::INTERPOL_FREQ] = 1;
        h[hw::SMOOTHING_MODE] = 1;
    }

    if old[hw::PROTOCOL] != h[hw::PROTOCOL] {
        if old[hw::STATUS] == 3 {
            // No header has been accepted yet: back to the defaults.
            (default_header(core_rate, false), 3)
        }
        else {
            // The old header stays in effect, and the frame is flagged as erroneous.
            let mut h = *old;
            h[hw::ERROR] = 1;
            h[hw::STATUS] = 2;
            (h, 2)
        }
    }
    else {
        // Only a change of these fields requires the tables to be rebuilt.
        const RESET_FIELDS: [usize; 6] =
            [hw::START, hw::STOP, hw::XOVER, hw::FREQ_SCALE, hw::ALTER_SCALE, hw::NOISE_BANDS];

        h[hw::FREQ_CHANGED] = i32::from(
            old[hw::START] != h[hw::START]
                || old[hw::STOP] != h[hw::STOP]
                || old[hw::FREQ_SCALE] != h[hw::FREQ_SCALE]
                || old[hw::ALTER_SCALE] != h[hw::ALTER_SCALE],
        );

        if RESET_FIELDS.iter().all(|&i| old[i] == h[i]) {
            h[hw::STATUS] = 0;
            (h, 0)
        }
        else {
            h[hw::STATUS] = 1;
            (h, 1)
        }
    }
}

/// The header words of a decoder that has not accepted a header yet. With `frame_data` the
/// frame-data error flag and the stereo LC mode are cleared as at the creation of a decoder.
fn default_header(core_rate: u32, frame_data: bool) -> [i32; HEADER_WORDS] {
    let mut h = DEFAULT_HEADER;
    if core_rate > 24000 {
        h[hw::ELEMENT_BIT] = 0;
    }
    if frame_data {
        h[hw::ERROR] = 0;
        h[hw::LC_STEREO_MODE] = 0;
    }
    h
}

// --- Grid and direction flags (spec section 7) ----------------------------------------------------

/// The number of bits of the transient pointer for `num_rel + 2` possible values.
fn pointer_bits(n: u32) -> u32 {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 3,
    }
}

/// Reads the frame class and its grid into `c.fi`. Returns `None` for an invalid grid.
fn extract_frame_info(r: &mut BitReader<'_>, c: &mut Chan) -> Option<()> {
    let fi = &mut c.fi;
    let class = r.get(2);

    match class {
        0 => {
            // FIXFIX.
            let num_env = 1u8 << r.get(2);
            let high_res = r.get(1) != 0;
            if !matches!(num_env, 1 | 2 | 4) {
                return None;
            }
            *fi = FrameInfo::fixfix(num_env);
            if !high_res {
                fi.freq_res[..usize::from(num_env)].fill(0);
            }
        }
        1 | 2 => {
            let abs_border = r.get(3) as i32;
            let num_rel = r.get(2) as usize;
            let num_env = num_rel + 1;

            if class == 1 {
                // FIXVAR: the borders are relative to the absolute border `abs_border + 9`, and
                // are read from the last to the first.
                fi.borders[0] = 0;
                let mut v = abs_border + TIME_SLOTS;
                fi.borders[num_env] = v as u8;
                for k in 0..num_rel {
                    v = v - 2 - 2 * r.get(2) as i32;
                    fi.borders[num_env - 1 - k] = v as u8;
                }
                let pointer = r.get(pointer_bits(num_rel as u32 + 2)) as usize;
                if num_env < pointer {
                    return None;
                }
                fi.transient = if pointer != 0 { (num_rel + 2 - pointer) as i32 } else { -1 };
                // The frequency resolutions are sent for the last envelope first.
                for e in (0..num_env).rev() {
                    fi.freq_res[e] = r.get(1) as u8;
                }
                fi.noise_borders[1] = if pointer < 2 {
                    fi.borders[num_rel]
                }
                else {
                    fi.borders[fi.transient as usize]
                };
            }
            else {
                // VARFIX.
                fi.borders[0] = abs_border as u8;
                let mut v = abs_border;
                for k in 1..=num_rel {
                    v = v + 2 + 2 * r.get(2) as i32;
                    fi.borders[k] = v as u8;
                }
                fi.borders[num_rel + 1] = TIME_SLOTS as u8;
                let pointer = r.get(pointer_bits(num_rel as u32 + 2)) as usize;
                if num_env < pointer {
                    return None;
                }
                fi.transient = if pointer >= 2 { pointer as i32 - 1 } else { -1 };
                for e in 0..num_env {
                    fi.freq_res[e] = r.get(1) as u8;
                }
                fi.noise_borders[1] = match pointer {
                    0 => fi.borders[1],
                    1 => fi.borders[num_rel],
                    _ => fi.borders[fi.transient as usize],
                };
            }
            fi.num_env = num_env as u8;
        }
        _ => {
            // VARVAR.
            let lead = r.get(3) as i32;
            let trail = r.get(3) as i32 + TIME_SLOTS;
            let n0 = r.get(2) as usize;
            let n1 = r.get(2) as usize;
            let num_rel = n0 + n1;
            let num_env = num_rel + 1;
            if num_env >= 5 {
                return None;
            }

            fi.borders[0] = lead as u8;
            let mut v = lead;
            for k in 1..=n0 {
                v = v + 2 + 2 * r.get(2) as i32;
                fi.borders[k] = v as u8;
            }
            fi.borders[num_rel + 1] = trail as u8;
            v = trail;
            for k in 0..n1 {
                v = v - 2 - 2 * r.get(2) as i32;
                fi.borders[num_env - 1 - k] = v as u8;
            }
            let pointer = r.get(pointer_bits(num_rel as u32 + 2)) as usize;
            if num_env < pointer {
                return None;
            }
            fi.transient = if pointer != 0 { (num_rel + 2 - pointer) as i32 } else { -1 };
            for e in 0..num_env {
                fi.freq_res[e] = r.get(1) as u8;
            }
            fi.noise_borders[0] = lead as u8;
            if num_env != 1 {
                fi.noise_borders[1] = if pointer >= 2 {
                    fi.borders[fi.transient as usize]
                }
                else {
                    fi.borders[num_rel]
                };
                fi.noise_borders[2] = trail as u8;
            }
            else {
                fi.noise_borders[1] = trail as u8;
            }
            fi.num_env = num_env as u8;
        }
    }

    if class != 0 {
        fi.num_noise = if fi.num_env != 1 { 2 } else { 1 };
        if class != 3 {
            fi.noise_borders[0] = fi.borders[0];
            fi.noise_borders[usize::from(fi.num_noise)] = fi.borders[usize::from(fi.num_env)];
        }
    }
    Some(())
}

/// Validates the grid of a channel. Returns `None` if it is invalid.
fn check_frame_info(c: &Chan) -> Option<()> {
    let fi = &c.fi;
    let num_env = usize::from(fi.num_env);
    let num_noise = usize::from(fi.num_noise);

    if !(1..=MAX_ENVELOPES).contains(&num_env) || num_noise > MAX_NOISE_ENVELOPES {
        return None;
    }

    let b = fi.borders.map(i32::from);
    let nb = fi.noise_borders.map(i32::from);
    let (first, last) = (b[0], b[num_env]);

    if !(first < last && first <= 3 && (9..=12).contains(&last)) {
        return None;
    }
    if (0..num_env).any(|i| b[i + 1] <= b[i]) {
        return None;
    }
    if !(fi.transient <= num_env as i32
        && (num_env != 1 || num_noise <= 1)
        && first == nb[0]
        && last == nb[num_noise])
    {
        return None;
    }
    if (0..num_noise).any(|i| nb[i + 1] <= nb[i]) {
        return None;
    }
    // Every noise border must coincide with an envelope border.
    if (0..num_noise).any(|i| !b[..num_env].contains(&nb[i])) {
        return None;
    }
    Some(())
}

/// Reads the envelope and noise-floor direction flags (1 = time, 0 = frequency).
fn get_direction_control(r: &mut BitReader<'_>, c: &mut Chan) {
    for e in 0..usize::from(c.fi.num_env) {
        c.dt_env[e] = r.get(1) as u8;
    }
    for n in 0..usize::from(c.fi.num_noise) {
        c.dt_noise[n] = r.get(1) as u8;
    }
}

// --- Envelope, noise floor, wave code, and extension data (spec section 9) -----------------------

/// Reads the raw envelope deltas of a channel (spec section 9).
fn get_envelope(r: &mut BitReader<'_>, c: &mut Chan) -> Option<()> {
    let amp_res = c.amp_res();
    let balance = c.coupling == COUPLING_BALANCE;
    let shift = u32::from(balance);
    let (start_bits, balance_bits) = if amp_res != 1 { (7, 6) } else { (6, 5) };

    let num_env = usize::from(c.fi.num_env);
    c.total = (0..num_env).map(|e| c.bands(e)).sum();
    if c.total > MAX_ENV_VALUES {
        return None;
    }

    let time_book = huffman::env_book(balance, amp_res, true);
    let freq_book = huffman::env_book(balance, amp_res, false);

    let mut offset = 0;
    for e in 0..num_env {
        let time = c.dt_env[e] != 0;
        let bands = c.bands(e);

        if !time {
            let start = if balance { r.get(balance_bits) << 1 } else { r.get(start_bits) };
            c.env[offset] = start as f32;
        }
        for j in usize::from(!time)..bands {
            let delta = huffman::decode(r, if time { time_book } else { freq_book });
            c.env[offset + j] = (delta << shift) as f32;
        }
        offset += bands;
    }
    Some(())
}

/// Reads the raw noise-floor deltas of a channel (spec section 9).
fn get_noise_floor(r: &mut BitReader<'_>, c: &mut Chan) {
    let balance = c.coupling == COUPLING_BALANCE;
    let shift = u32::from(balance);
    let nq = c.nq;

    for n in 0..usize::from(c.fi.num_noise) {
        let base = n * nq;
        if c.dt_noise[n] != 0 {
            let book = huffman::noise_book(balance, true);
            for j in 0..nq {
                c.noise[base + j] = (huffman::decode(r, book) << shift) as f32;
            }
        }
        else {
            let start = r.get(5);
            c.noise[base] = (start << shift) as f32;
            let book = huffman::noise_book(balance, false);
            for j in 1..nq {
                c.noise[base + j] = (huffman::decode(r, book) << shift) as f32;
            }
        }
    }
}

/// Parses and discards the wave code data of a channel. Returns `None` for the invalid mode 0.
///
/// The data is only parsed to keep the bit position. Its two band ranges are `[0, 9)`/`[9, 18)`
/// selected by the mode bits.
fn skip_wave_code(r: &mut BitReader<'_>) -> Option<()> {
    if r.get(1) == 0 {
        return Some(());
    }

    for _ in 0..=r.get(2) {
        r.skip(6);
        let blocks = r.get(2) + 1;
        let mode = r.get(2);
        if mode == 0 {
            return None;
        }
        let lo = if mode & 2 != 0 { 0 } else { 9 };
        let hi = if mode & 1 != 0 { 18 } else { 9 };
        r.skip(1);
        for _ in 0..blocks {
            let width = r.get(1) + 4;
            r.skip(5);
            for _ in lo..hi {
                if r.get(width - 1) != 0 {
                    r.skip(1);
                }
            }
        }
    }
    Some(())
}

/// Skips the extended data: a flag, a 4-bit count (escape 15 adds 8 more bits), and the payload.
fn skip_extension(r: &mut BitReader<'_>) {
    if r.get(1) != 0 {
        let mut count = r.get(4) as usize;
        if count == 15 {
            count += r.get(8) as usize;
        }
        r.skip(count * 8);
    }
}

// --- Elements (spec section 6) --------------------------------------------------------------------

/// Reads the inverse filtering modes (shared by both channels if `coupled`), the 2-bit extra
/// field(s), and the `data_extra` field(s).
fn get_invf(r: &mut BitReader<'_>, c0: &mut Chan, c1: &mut Chan, coupled: bool) {
    if coupled {
        if c0.has_invf {
            let v = r.get(2);
            c0.invf[0] = v;
            c1.invf[0] = v;
        }
        r.skip(2);
        if c0.hw[hw::DATA_EXTRA] != 0 {
            let v = r.get(3);
            c0.data_extra = v;
            c1.data_extra = v;
        }
    }
    else {
        if c0.has_invf {
            c0.invf[0] = r.get(2);
        }
        if c1.has_invf {
            c1.invf[0] = r.get(2);
        }
        r.skip(4);
        if c0.hw[hw::DATA_EXTRA] != 0 {
            c0.data_extra = r.get(3);
            c1.data_extra = r.get(3);
        }
    }
}

/// Parses a channel pair element. Returns `None` if it is invalid.
fn get_channel_pair_element(r: &mut BitReader<'_>, c0: &mut Chan, c1: &mut Chan) -> Option<()> {
    let previous = c0.hw[hw::ELEMENT_BIT];
    let bit = r.get(1) as i32;
    c0.hw[hw::ELEMENT_BIT] = bit;
    c1.hw[hw::ELEMENT_BIT] = bit;
    if previous != bit {
        return None;
    }

    let coupled = r.get(1) != 0;
    (c0.coupling, c1.coupling) =
        if coupled { (COUPLING_LEVEL, COUPLING_BALANCE) } else { (COUPLING_NONE, COUPLING_NONE) };

    extract_frame_info(r, c0)?;
    check_frame_info(c0)?;
    if coupled {
        // The second channel shares the grid and the error flag.
        c1.hw[hw::ERROR] = c0.hw[hw::ERROR];
        c1.fi = c0.fi;
    }
    else {
        extract_frame_info(r, c1)?;
        check_frame_info(c1)?;
    }

    get_direction_control(r, c0);
    get_direction_control(r, c1);

    // Frequency-coded first envelopes in both channels end an error state.
    if c0.dt_env[0] == 0 && c1.dt_env[0] == 0 {
        c0.hw[hw::ERROR] = 0;
        c1.hw[hw::ERROR] = 0;
    }

    get_invf(r, c0, c1, coupled);

    if coupled {
        get_envelope(r, c0)?;
        get_noise_floor(r, c0);
        get_envelope(r, c1)?;
        get_noise_floor(r, c1);
    }
    else {
        get_envelope(r, c0)?;
        get_envelope(r, c1)?;
        get_noise_floor(r, c0);
        get_noise_floor(r, c1);
    }

    skip_wave_code(r)?;
    skip_wave_code(r)?;
    skip_extension(r);
    if !coupled {
        skip_extension(r);
    }
    Some(())
}

/// Parses a single channel element. Returns `None` if it is invalid.
fn get_single_channel_element(r: &mut BitReader<'_>, c: &mut Chan) -> Option<()> {
    c.coupling = COUPLING_NONE;

    let previous = c.hw[hw::ELEMENT_BIT];
    let bit = r.get(1) as i32;
    c.hw[hw::ELEMENT_BIT] = bit;
    if previous != bit {
        return None;
    }

    // The LC stereo enable bit.
    let lc = r.get(1) != 0;
    if c.hw[hw::ELEMENT_KIND] == 0 {
        c.hw[hw::ELEMENT_KIND] = if lc { 3 } else { 1 };
    }

    extract_frame_info(r, c)?;
    check_frame_info(c)?;
    get_direction_control(r, c);
    if c.dt_env[0] == 0 {
        c.hw[hw::ERROR] = 0;
    }

    if c.has_invf {
        c.invf[0] = r.get(2);
    }
    r.skip(2);
    if c.hw[hw::DATA_EXTRA] != 0 {
        c.data_extra = r.get(3);
    }

    get_envelope(r, c)?;
    get_noise_floor(r, c);
    skip_wave_code(r)?;
    skip_extension(r);
    Some(())
}

// --- Delta decoding (spec section 11.6) -------------------------------------------------------------

/// The clamp of the envelope exponents.
fn envelope_limit(amp_res: i32) -> f32 {
    if amp_res == 1 { 35.0 } else { 70.0 }
}

/// The clamp of the noise-floor exponents.
const NOISE_LIMIT: f32 = 35.0;

/// Stores `val` for band `j` of the envelope's frequency table in the high-resolution memory: one
/// entry for a high-resolution band, two for a low-resolution band.
fn store_previous(c: &mut Chan, j: usize, high: bool, val: f32) {
    let prev = &mut c.prev.env;
    if high || j < c.v4 {
        prev[j] = val;
    }
    else {
        prev[2 * j - c.v4] = val;
        prev[2 * j - c.v4 + 1] = val;
    }
}

/// The high-resolution memory index of band `j` of a low-resolution envelope.
fn previous_index(c: &Chan, j: usize, high: bool) -> usize {
    if high || j < c.v4 { j } else { 2 * j - c.v4 }
}

/// Converts the envelope deltas to absolute exponents (frequency direction: running sum; time
/// direction: added to the previous frame's value) and updates the previous-frame memory.
fn delta_to_exponent(c: &mut Chan) {
    let mut k = 0;
    for e in 0..usize::from(c.fi.num_env) {
        let high = c.fi.freq_res[e] != 0;
        let bands = c.bands(e);

        if c.dt_env[e] != 0 {
            for j in 0..bands {
                let prev = c.prev.env[previous_index(c, j, high)];
                let v = (f64::from(c.env[k]) + f64::from(prev)) as f32;
                c.env[k] = v;
                store_previous(c, j, high, v);
                k += 1;
            }
        }
        else {
            store_previous(c, 0, high, c.env[k]);
            k += 1;
            for j in 1..bands {
                let v = (f64::from(c.env[k]) + f64::from(c.env[k - 1])) as f32;
                c.env[k] = v;
                store_previous(c, j, high, v);
                k += 1;
            }
        }
    }
}

/// Clamps negative exponents to zero (in the frame, and in the memory with an upper clamp too).
/// Returns true if a frame exponent exceeds the limit.
fn check_envelope(c: &mut Chan) -> bool {
    let limit = envelope_limit(c.amp_res());
    let mut error = false;

    for v in c.env[..c.total].iter_mut() {
        error |= *v > limit;
        if *v < 0.0 {
            *v = 0.0;
        }
    }
    for v in c.prev.env[..c.n_high].iter_mut() {
        if *v < 0.0 {
            *v = 0.0;
        }
        else if *v > limit {
            *v = limit;
        }
    }
    error
}

/// Moves the start border of the first envelope to the end border of the previous frame minus 9
/// (spec section 11.6). A frequency-coded envelope that is not a coupling balance gets the
/// resulting log2 length change added to *every* delta.
fn compensate_first_envelope(c: &mut Chan) {
    let v5 = c.prev_end - TIME_SLOTS;
    let (b0, b1) = (i32::from(c.fi.borders[0]), i32::from(c.fi.borders[1]));

    let ratio = f64::from(b1 - b0) / f64::from(b1 - v5);
    let log2 = (ratio.ln() / std::f64::consts::LN_2) as f32;
    let log2 = if c.amp_res() == 0 { f64::from(log2) * 2.0 } else { f64::from(log2) };

    c.fi.borders[0] = v5 as u8;
    c.fi.noise_borders[0] = v5 as u8;

    if c.coupling != COUPLING_BALANCE {
        let n = c.bands(0);
        for v in c.env[..n].iter_mut() {
            *v = (f64::from(*v) + log2) as f32;
        }
    }
}

/// Replaces the data of a channel with a concealment frame: a single time-coded envelope that
/// steps every exponent of the previous frame by one unit towards 0 (by half a unit towards 12 for
/// a balance channel). The noise floor repeats the previous frame's.
fn conceal(c: &mut Chan) {
    let slots = TIME_SLOTS as u8;
    let start = (c.prev_end - TIME_SLOTS) as u8;

    c.coupling = c.prev.coupling;
    if c.has_invf {
        c.invf[0] = c.invf[1];
    }

    c.fi.num_env = 1;
    c.fi.borders[0] = start;
    c.fi.borders[1] = slots;
    c.fi.freq_res[0] = 1;
    c.fi.transient = -1;
    c.fi.num_noise = 1;
    c.fi.noise_borders[0] = start;
    c.fi.noise_borders[1] = slots;

    c.total = c.n_high;
    c.dt_env[0] = 1;
    c.dt_noise[0] = 1;

    let (step, target) = if c.coupling == COUPLING_BALANCE { (0.5f32, 12.0) } else { (1.0, 0.0) };
    for i in 0..c.total {
        c.env[i] = if target < f64::from(c.prev.env[i]) { -step } else { step };
    }
    c.noise[..c.nq].fill(0.0);

    delta_to_exponent(c);
}

/// Converts the exponents to linear energies: `64 * 2^(x / (2 - amp_res))`.
fn requantize_envelope(c: &mut Chan) {
    let scale = f64::from((1.0 / (2.0 - f64::from(c.amp_res()))) as f32);
    for v in c.env[..c.total].iter_mut() {
        *v = (64.0 * (scale * f64::from(*v)).exp2()) as f32;
    }
}

/// Decodes the envelope of a channel: concealment, border correction, coupling memory
/// re-interpretation, delta decoding, validation, and requantisation. `other` is the previous
/// frame envelope memory of the other channel.
fn decode_envelope(c: &mut Chan, other: &[f32; MAX_BANDS_HIGH]) {
    // A first envelope that does not start where the previous frame ended is an error if it is
    // time-coded, and needs a border correction if it is frequency-coded.
    if !c.prev.flag && !c.error() && i32::from(c.fi.borders[0]) != c.prev_end - TIME_SLOTS {
        if c.dt_env[0] == 1 {
            c.hw[hw::ERROR] = 1;
        }
        else {
            c.prev.flag = true;
        }
    }

    if !c.error() {
        if c.prev.flag {
            compensate_first_envelope(c);

            // The memory of the previous frame is re-interpreted if the coupling changed.
            if c.coupling != c.prev.coupling {
                let (coupling, prev_coupling) = (c.coupling, c.prev.coupling);
                for (own, &other) in c.prev.env[..c.n_high].iter_mut().zip(other) {
                    *own = if prev_coupling == COUPLING_BALANCE {
                        other
                    }
                    else if coupling == COUPLING_LEVEL {
                        ((f64::from(*own) + f64::from(other)) * 0.5) as f32
                    }
                    else if coupling == COUPLING_BALANCE {
                        12.0
                    }
                    else {
                        *own
                    };
                }
            }
        }

        let backup = c.prev.env;
        delta_to_exponent(c);
        if check_envelope(c) {
            c.hw[hw::ERROR] = 1;
            c.prev.env = backup;
        }
    }

    if c.error() {
        conceal(c);
    }

    requantize_envelope(c);
    c.prev.flag = c.error();
    c.prev.coupling = c.coupling;
    c.prev_end = i32::from(c.fi.borders[usize::from(c.fi.num_env)]);
}

/// Decodes the noise-floor exponents of a channel and converts them to linear noise-to-signal
/// ratios `2^(6 - n)` (uncoupled channels only).
fn decode_noise_floor(c: &mut Chan) {
    let nq = c.nq;
    let num_noise = usize::from(c.fi.num_noise);
    let total = num_noise * nq;
    let add = |a: f32, b: f32| (f64::from(a) + f64::from(b)) as f32;

    if c.dt_noise[0] == 0 {
        for i in 1..nq {
            c.noise[i] = add(c.noise[i], c.noise[i - 1]);
        }
    }
    else {
        for i in 0..nq {
            c.noise[i] = add(c.noise[i], c.prev.noise[i]);
        }
    }
    if num_noise > 1 {
        if c.dt_noise[1] == 0 {
            for i in nq + 1..total {
                c.noise[i] = add(c.noise[i], c.noise[i - 1]);
            }
        }
        else {
            for i in 0..nq {
                c.noise[nq + i] = add(c.noise[nq + i], c.noise[i]);
            }
        }
    }

    for v in c.noise[..total].iter_mut() {
        *v = v.clamp(0.0, NOISE_LIMIT);
    }
    c.prev.noise[..nq].copy_from_slice(&c.noise[total - nq..total]);

    if c.coupling == COUPLING_NONE {
        for v in c.noise[..total].iter_mut() {
            *v = (6.0 - f64::from(*v)).exp2() as f32;
        }
    }
}

/// Converts the coupled level/balance data of the channel pair to the energies and noise ratios
/// of the left (`c0`) and right (`c1`) channel (spec section 11.6).
fn unmap_coupling(c0: &mut Chan, c1: &mut Chan) {
    // The right channel is computed first, then the left one is derived from it.
    for i in 0..c0.total {
        let t = f64::from(c1.env[i]) * (1.0 / 4096.0) / 64.0;
        let right = ((f64::from(c0.env[i]) + f64::from(c0.env[i])) / (1.0 + t)) as f32;
        c1.env[i] = right;
        c0.env[i] = (f64::from(right) * t) as f32;
    }

    for i in 0..usize::from(c0.fi.num_noise) * c0.nq {
        // The "- 1" in the level exponent is a deviation of the mp3PRO decoder from the standard.
        let p1 = f64::from(((6.0 - f64::from(c0.noise[i])) - 1.0) as f32).exp2() as f32;
        let p2 = (12.0 - f64::from(c1.noise[i])).exp2() as f32;
        c0.noise[i] = ((f64::from(p2) + 1.0) * f64::from(p1)) as f32;
        c1.noise[i] = (f64::from(p1) * (1.0 / f64::from(p2) + 1.0)) as f32;
    }
}

/// Decodes the envelopes and noise floors of both channels (or the single channel `c0`).
fn decode_sbr_data(c0: &mut Chan, c1: Option<&mut Chan>) {
    let other = c1.as_ref().map_or([0.0; MAX_BANDS_HIGH], |c| c.prev.env);
    decode_envelope(c0, &other);
    decode_noise_floor(c0);

    let Some(c1) = c1
    else {
        return;
    };

    if c0.error() {
        c1.hw[hw::ERROR] = 1;
    }
    decode_envelope(c1, &c0.prev.env);
    decode_noise_floor(c1);

    if !c0.error() && c1.error() {
        // The error of the second channel is propagated to the first by decoding it again.
        c0.hw[hw::ERROR] = 1;
        decode_envelope(c0, &c1.prev.env);
    }

    if c0.coupling != COUPLING_NONE {
        unmap_coupling(c0, c1);
    }
}

/// Converts the decoded data of a channel to the parameters of the DSP stages.
fn to_frame(c: &Chan) -> SbrFrame {
    let fi = &c.fi;
    let mut frame = SbrFrame {
        num_env: usize::from(fi.num_env),
        borders: fi.borders,
        l_a: fi.transient as i8,
        num_noise: usize::from(fi.num_noise),
        noise_borders: fi.noise_borders,
        invf: c.invf[0] as u8,
        data_extra: c.data_extra as u8,
        ..SbrFrame::default()
    };
    for (dst, &src) in frame.freq_res.iter_mut().zip(&fi.freq_res) {
        *dst = src != 0;
    }

    let mut offset = 0;
    for e in 0..frame.num_env {
        let bands = c.bands(e);
        frame.env[e][..bands].copy_from_slice(&c.env[offset..offset + bands]);
        offset += bands;
    }
    for n in 0..frame.num_noise {
        frame.noise[n][..c.nq].copy_from_slice(&c.noise[n * c.nq..(n + 1) * c.nq]);
    }

    frame
}

/// The tables of a decoder for which no valid default tables exist.
fn empty_tables() -> FreqTables {
    FreqTables {
        kx: 0,
        k2: 0,
        f_hi: [0; MAX_BANDS_HIGH + 1],
        n_high: 0,
        f_lo: [0; super::MAX_BANDS_LOW + 1],
        n_low: 0,
        f_noise: [0; MAX_NOISE_BANDS + 1],
        nq: 0,
        f_lim: [0; super::MAX_LIMITER_BANDS + 1],
        nl: 0,
    }
}

/// The mp3PRO SBR bitstream parser.
///
/// The parser keeps the state that links consecutive frames: the active header, the envelope and
/// noise-floor memory used by time-coded data, the coupling state, and whether SBR has been
/// switched off.
pub(crate) struct SbrParser {
    n_channels: usize,
    /// The sample rate of the SBR output (twice the core rate).
    fs_out: u32,
    chans: [Chan; 2],
    /// Per channel: 0 = no header seen, 1 = no SBR data in effect, 2 = SBR active.
    state: [u8; 2],
    disabled: bool,
    tables: FreqTables,
    header: SbrHeader,
    frames: [SbrFrame; 2],
}

impl SbrParser {
    /// Creates a parser for a stream of `n_channels` (1 or 2) channels with the sample rate
    /// `core_rate` of the MP3 core. The default header (start 5, stop 0) is in effect until the
    /// first header is parsed.
    pub fn new(core_rate: u32, n_channels: usize) -> Self {
        let n_channels = n_channels.clamp(1, 2);
        let fs_out = core_rate * 2;

        let hw = default_header(core_rate, true);
        let header = header_of(&hw);
        let tables = freq::tables(&header, fs_out).unwrap_or_else(empty_tables);
        let chan = Chan::new(hw, &tables);

        SbrParser {
            n_channels,
            fs_out,
            chans: [chan, chan],
            state: [0; 2],
            disabled: false,
            tables,
            header,
            frames: [SbrFrame::default(); 2],
        }
    }

    /// Parses the ancillary data `anc` of one frame.
    ///
    /// `anc` starts at the first byte of the ancillary data region and may extend beyond its end:
    /// the block length field of the SBR block bounds the data that is read.
    pub fn parse(&mut self, anc: &[u8]) -> ParseStatus {
        let mut status = ParseStatus::default();

        if self.disabled {
            status.disabled = true;
            return status;
        }

        let ret = self.read_sbr(anc, &mut status);
        self.header = header_of(&self.chans[0].hw);

        if ret != 0 {
            // Any failure switches SBR off for good. The frame itself still takes the core path.
            self.disabled = true;
            status.disabled = true;
            status.header_reset = false;
            self.state = [1; 2];
            return status;
        }

        if self.state[0] != 2 {
            self.state = [1; 2];
            return status;
        }

        let [c0, c1] = &mut self.chans;
        if self.n_channels == 2 {
            decode_sbr_data(c0, Some(c1));
        }
        else {
            decode_sbr_data(c0, None);
        }

        for ch in 0..self.n_channels {
            let c = &mut self.chans[ch];
            self.frames[ch] = to_frame(c);
            // The inverse filtering mode of this frame becomes the previous one.
            if c.has_invf {
                c.invf[1] = c.invf[0];
            }
        }
        status.apply = true;
        status.clean = self.chans[..self.n_channels].iter().all(|c| c.hw[hw::ERROR] == 0);
        status
    }

    /// The header in effect after the last parsed frame.
    pub fn header(&self) -> &SbrHeader {
        &self.header
    }

    /// The frequency tables of the header in effect.
    pub fn tables(&self) -> &FreqTables {
        &self.tables
    }

    /// The parameters of channel `ch` of the last frame. Valid if [`ParseStatus::apply`] was set.
    pub fn frame(&self, ch: usize) -> &SbrFrame {
        &self.frames[ch]
    }

    /// Rebuilds the band tables for the header of channel `ch`. Returns false if the header is
    /// rejected.
    fn reset_dec(&mut self, ch: usize) -> bool {
        let header = header_of(&self.chans[ch].hw);

        match freq::tables(&header, self.fs_out) {
            Some(tables) => {
                let c = &mut self.chans[ch];
                c.hw[hw::NQ] = tables.nq as i32;
                c.n_low = tables.n_low;
                c.n_high = tables.n_high;
                c.nq = tables.nq;
                c.has_invf = true;
                self.tables = tables;
                true
            }
            None => false,
        }
    }

    /// Marks the data of all channels as erroneous (concealment is applied when decoding).
    fn flag_errors(&mut self) {
        for c in self.chans[..self.n_channels].iter_mut() {
            c.hw[hw::ERROR] = 1;
        }
    }

    /// Reads the framing, the header, and the element of one block. Returns 0, or the code of a
    /// failure that switches SBR off.
    fn read_sbr(&mut self, anc: &[u8], status: &mut ParseStatus) -> u32 {
        let n_channels = self.n_channels;

        // The block length (excluding the two bytes of the length field) bounds the data.
        let b0 = usize::from(anc.first().copied().unwrap_or(0));
        let b1 = usize::from(anc.get(1).copied().unwrap_or(0));
        let length = (((b0 & 0x1f) << 4) | (b1 >> 4)) + 2;
        let block = &anc[..anc.len().min(length)];

        let mut r = BitReader::new(block);

        // The block starts with the bits `11` in every mp3PRO stream observed (the first byte of
        // the block is 0xc0 or 0xe0 with the header flag). Leftover bytes of a plain MP3 stream
        // rarely have them, and the CRC and the element checks below are weak on their own.
        if r.get(2) != 0b11 {
            return ERR_PROTOCOL;
        }
        let header_flag = r.get(1) != 0;
        r.skip(9);

        // The CRC covers the 108 (140 with a header) bits that follow it, or fewer at the end of
        // the block.
        if r.get(1) != 0 {
            let want = r.get(7);
            let available = (block.len() * 8).saturating_sub(r.pos());
            let n = available.min(if header_flag { 140 } else { 108 });
            if crc7(block, r.pos(), n) != want {
                self.flag_errors();
                return if self.state[0] == 0 { ERR_PROTOCOL } else { 0 };
            }
        }

        let mut mismatch = false;
        if header_flag {
            let old = self.chans[0].hw;
            let (hw, ret) = parse_header(&mut r, &old, n_channels, self.fs_out / 2);
            for c in self.chans[..n_channels].iter_mut() {
                c.hw = hw;
            }
            if ret == 3 {
                return ERR_PROTOCOL;
            }

            self.state = [2; 2];
            mismatch = ret == 2;

            if ret == 1 {
                status.header_reset = true;
                for ch in 0..n_channels {
                    if !self.reset_dec(ch) {
                        // The header is rejected: the previous one stays in effect (the
                        // frequency-changed word keeps its new value) and SBR is inactive
                        // until the next header.
                        let c = &mut self.chans[ch];
                        let freq_changed = c.hw[hw::FREQ_CHANGED];
                        c.hw = old;
                        c.hw[hw::FREQ_CHANGED] = freq_changed;
                        self.state[ch] = 1;
                        if !self.reset_dec(ch) {
                            return ERR_RESET;
                        }
                    }
                }
            }
        }

        if self.state[0] == 2 {
            let kind = self.chans[0].hw[hw::ELEMENT_KIND];
            if (n_channels == 2 && kind != 2) || (n_channels == 1 && kind == 2) {
                return ERR_PROTOCOL;
            }

            let [c0, c1] = &mut self.chans;
            let ok = if n_channels == 2 {
                get_channel_pair_element(&mut r, c0, c1)
            }
            else {
                get_single_channel_element(&mut r, c0)
            };

            // The element must end in the last byte of the block.
            let complete = r.pos().div_ceil(8) == block.len();
            if ok.is_none() || !complete || mismatch {
                self.flag_errors();
            }
        }

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_crc7() {
        let data: Vec<u8> = (1..20).collect();
        assert_eq!(crc7(&data, 3, 100), 68);
        assert_eq!(crc7(&data, 0, 8), 118);
    }

    #[test]
    fn verify_fixvar_grid_order() {
        // Class FIXVAR, absolute border 2, 3 envelopes: the relative borders are sent from the
        // last to the first, the transient pointer follows, and the frequency resolutions are
        // sent for the last envelope first.
        let bits = "01" /* class */ .to_owned()
            + "010" // absolute border
            + "10" // number of relative borders
            + "01" + "00" // relative borders, last first
            + "10" // pointer
            + "100"; // frequency resolutions, last envelope first
        let mut data = [0u8; 4];
        for (i, b) in bits.bytes().enumerate() {
            data[i / 8] |= (b - b'0') << (7 - i % 8);
        }

        let tables = freq::tables(&header_of(&default_header(22050, true)), 44100).unwrap();
        let mut chan = Chan::new(default_header(22050, true), &tables);
        let mut r = BitReader::new(&data);
        assert!(extract_frame_info(&mut r, &mut chan).is_some());

        let fi = &chan.fi;
        assert_eq!(fi.num_env, 3);
        assert_eq!(fi.borders[..4], [0, 5, 7, 11]);
        assert_eq!(fi.freq_res[..3], [0, 0, 1]);
        assert_eq!(fi.transient, 2);
        assert_eq!(fi.num_noise, 2);
        assert_eq!(fi.noise_borders, [0, 7, 11]);
    }

    #[test]
    fn default_tables_exist_for_all_core_rates() {
        for rate in [8000, 11025, 12000, 16000, 22050, 24000] {
            let parser = SbrParser::new(rate, 2);
            assert!(parser.tables().n_high > 0 && parser.tables().nq > 0, "rate {rate}");
        }
    }
}
