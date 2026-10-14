// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IEC 61937 (S/PDIF) burst parsing.
//!
//! A compressed audio stream (AC-3, E-AC-3, DTS) may be carried as the samples of a 16-bit stereo
//! PCM stream. Each *burst* starts with a preamble of four 16-bit words, followed by the payload
//! (a frame of the compressed stream, in 16-bit words) and zero padding up to the *repetition
//! period* of the data type:
//!
//! ```text
//! Pa = 0xF872   Pb = 0x4E1F   Pc = burst info   Pd = payload length
//! ```
//!
//! In a WAVE file the words are little-endian, in which case the byte order of the payload words
//! is also swapped relative to the compressed stream. Big-endian carriers (e.g., AIFF) are also
//! recognised.

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3};

/// The length of the burst preamble (`Pa`, `Pb`, `Pc`, `Pd`) in bytes.
pub const BURST_PREAMBLE_LEN: usize = 8;

/// The first two synchronisation words as they appear in a stream of little-endian words.
pub const SYNC_LE: [u8; 4] = [0x72, 0xf8, 0x1f, 0x4e];
/// The first two synchronisation words as they appear in a stream of big-endian words.
pub const SYNC_BE: [u8; 4] = [0xf8, 0x72, 0x4e, 0x1f];

/// The largest repetition period handled, in bytes (E-AC-3).
const MAX_STRIDE: usize = 24576;

/// The parsed preamble of a burst.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Burst {
    /// The codec of the payload.
    pub codec: AudioCodecId,
    /// The length of the payload in bytes (always a multiple of 2).
    pub payload_len: usize,
    /// The distance in bytes from the start of this burst's preamble to the start of the next.
    pub stride: usize,
    /// True if the 16-bit words of the carrier are little-endian.
    pub little_endian: bool,
}

/// Maps the data type in `Pc` to a codec and the repetition period in bytes (the period is in
/// 16-bit stereo sample frames of 4 bytes).
fn data_type_info(data_type: u8) -> Option<(AudioCodecId, usize)> {
    match data_type {
        // AC-3: 1536 samples.
        0x01 => Some((CODEC_ID_AC3, 1536 * 4)),
        // DTS type I, II, III: 512, 1024, 2048 samples.
        0x0b => Some((CODEC_ID_DCA, 512 * 4)),
        0x0c => Some((CODEC_ID_DCA, 1024 * 4)),
        0x0d => Some((CODEC_ID_DCA, 2048 * 4)),
        // E-AC-3: 6144 samples.
        0x15 => Some((CODEC_ID_EAC3, 6144 * 4)),
        _ => None,
    }
}

/// Returns true if `buf` begins with the two synchronisation words, and if so whether the carrier
/// is little-endian.
fn sync_order(buf: &[u8]) -> Option<bool> {
    match buf.get(..4)? {
        w if w == SYNC_LE => Some(true),
        w if w == SYNC_BE => Some(false),
        _ => None,
    }
}

/// Parses the preamble at the start of `buf` (at least [`BURST_PREAMBLE_LEN`] bytes). Returns
/// `None` if `buf` does not begin with the synchronisation words or the data type is not supported.
pub fn parse_burst(buf: &[u8]) -> Option<Burst> {
    if buf.len() < BURST_PREAMBLE_LEN {
        return None;
    }

    let little_endian = sync_order(buf)?;
    let word = |i: usize| {
        let b = [buf[i], buf[i + 1]];
        if little_endian { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
    };

    let pc = word(4);
    let pd = usize::from(word(6));

    // The error flag (bit 7) marks the payload as invalid. Only the data type is otherwise used.
    let data_type = u8::try_from(pc).ok()?;
    let (codec, stride) = data_type_info(data_type)?;

    // `Pd` is the length in bits, except for E-AC-3 where it is in bytes.
    let payload_len = if codec == CODEC_ID_EAC3 { pd } else { pd.div_ceil(16) * 2 };

    // The payload plus preamble must fit in the repetition period.
    if payload_len + BURST_PREAMBLE_LEN > stride {
        return None;
    }

    Some(Burst { codec, payload_len, stride, little_endian })
}

/// Converts the payload of a burst to the byte order of the compressed stream.
pub fn unwrap_payload(payload: &mut [u8], little_endian: bool) {
    if little_endian {
        for w in payload.chunks_exact_mut(2) {
            w.swap(0, 1);
        }
    }
}

/// The result of probing a stream for IEC 61937 bursts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub codec: AudioCodecId,
}

/// Probes the start of a 16-bit stereo PCM stream for a stream of IEC 61937 bursts.
///
/// As with the reference implementation, a stream is only identified as IEC 61937 when three
/// consecutive bursts are found at the distance expected from the data type.
pub fn probe(buf: &[u8]) -> Option<Probe> {
    let end = buf.len().min(2 * MAX_STRIDE);

    let mut pos = 0;

    while pos + BURST_PREAMBLE_LEN <= end {
        // Find the next sync word.
        let off = buf[pos..end].windows(4).position(|w| w == SYNC_LE || w == SYNC_BE)?;

        pos += off;

        // Follow the chain of bursts from here.
        let mut chain = 0;
        let mut at = pos;
        let mut codec = None;

        while let Some(burst) = buf.get(at..).and_then(parse_burst) {
            if *codec.get_or_insert(burst.codec) != burst.codec {
                break;
            }
            chain += 1;
            if chain >= 3 {
                return Some(Probe { codec: burst.codec });
            }
            at += burst.stride;
        }

        pos += 1;
    }

    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a little-endian burst of `stride` bytes carrying `payload` (stream byte order).
    pub(crate) fn make_burst(data_type: u16, payload: &[u8], stride: usize, le: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let (pa, pb) = (0xf872u16, 0x4e1fu16);
        let bits = if data_type == 0x15 { payload.len() } else { payload.len() * 8 };
        for w in [pa, pb, data_type, bits as u16] {
            out.extend_from_slice(&if le { w.to_le_bytes() } else { w.to_be_bytes() });
        }
        let mut body = payload.to_vec();
        if body.len() % 2 == 1 {
            body.push(0);
        }
        unwrap_payload(&mut body, le);
        out.extend_from_slice(&body);
        out.resize(stride, 0);
        out
    }

    #[test]
    fn parses_ac3_burst() {
        let b = make_burst(0x01, &[0x0b, 0x77, 1, 2, 3, 4], 6144, true);
        assert_eq!(&b[..4], &SYNC_LE);
        let burst = parse_burst(&b).unwrap();
        assert_eq!(burst.codec, CODEC_ID_AC3);
        assert_eq!(burst.payload_len, 6);
        assert_eq!(burst.stride, 6144);
        assert!(burst.little_endian);

        let mut payload = b[8..8 + burst.payload_len].to_vec();
        unwrap_payload(&mut payload, true);
        assert_eq!(payload, [0x0b, 0x77, 1, 2, 3, 4]);
    }

    #[test]
    fn parses_big_endian_dts_burst() {
        let b = make_burst(0x0c, &[0x7f, 0xfe, 0x80, 0x01], 4096, false);
        assert_eq!(&b[..4], &SYNC_BE);
        let burst = parse_burst(&b).unwrap();
        assert_eq!(burst.codec, CODEC_ID_DCA);
        assert_eq!(burst.stride, 4096);
        assert!(!burst.little_endian);
    }

    #[test]
    fn eac3_length_is_in_bytes() {
        let b = make_burst(0x15, &[0x0b, 0x77, 0, 0, 0, 0, 0, 0], 24576, true);
        let burst = parse_burst(&b).unwrap();
        assert_eq!(burst.codec, CODEC_ID_EAC3);
        assert_eq!(burst.payload_len, 8);
        assert_eq!(burst.stride, 24576);
    }

    #[test]
    fn rejects_unknown_type_and_oversize() {
        let b = make_burst(0x16, &[0, 0], 6144, true);
        assert!(parse_burst(&b).is_none());
        // Payload longer than the period.
        let mut b = make_burst(0x01, &[0, 0], 6144, true);
        b[6] = 0xff;
        b[7] = 0xff;
        assert!(parse_burst(&b).is_none());
    }

    #[test]
    fn probe_needs_three_bursts() {
        let burst = make_burst(0x01, &[0x0b, 0x77, 0, 0], 6144, true);
        let mut data = vec![0u8; 100];
        data.extend_from_slice(&burst);
        data.extend_from_slice(&burst);
        assert!(probe(&data).is_none());
        data.extend_from_slice(&burst);
        assert_eq!(probe(&data), Some(Probe { codec: CODEC_ID_AC3 }));
        assert!(probe(&[0u8; 4096]).is_none());
    }
}
