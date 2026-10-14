// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MPEG Packetized Elementary Stream (PES) header parsing (ISO/IEC 13818-1 §2.4.3.6, ISO/IEC
//! 11172-1 §2.4.3.3), and helpers for the 33-bit, 90 kHz, PTS and DTS timestamps.

/// The clock frequency of presentation and decode timestamps in Hz.
pub const PTS_CLOCK_HZ: u64 = 90_000;

/// Presentation and decode timestamps are 33 bits wide, and wrap at this value.
pub const PTS_MODULUS: u64 = 1 << 33;

/// The maximum length of a PES header, including the start code, that [`parse_pes_header`] needs to
/// examine.
pub const PES_MAX_HEADER_LEN: usize = 6 + 3 + 255;

/// Returns true if PES packets with the stream ID have the optional PES header (and therefore, may
/// carry timestamps).
pub fn stream_id_has_header(stream_id: u8) -> bool {
    !matches!(
        stream_id,
        // Program stream map, padding, private stream 2, ECM, EMM, program stream directory,
        // DSMCC, H.222.1 type E, and the prohibited stream ID.
        0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff
    )
}

/// The parsed header of a PES packet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PesHeader {
    /// The stream ID.
    pub stream_id: u8,
    /// The value of the `PES_packet_length` field. The number of bytes following the field, or 0
    /// if the length is unbounded (only allowed for video elementary streams in transport
    /// streams).
    pub packet_len: usize,
    /// The presentation timestamp, if present.
    pub pts: Option<u64>,
    /// The decode timestamp, if present.
    pub dts: Option<u64>,
    /// The `stream_id_extension` of the PES extension (ISO/IEC 13818-1 §2.4.3.7), if present. The
    /// stream ID is then `0xfd` (extended stream ID). Blu-ray distinguishes the TrueHD (`0x72`)
    /// and the embedded AC-3 (`0x76`) parts of a combined stream with it.
    pub extended_stream_id: Option<u8>,
    /// The offset of the first byte of the payload from the first byte of the start code.
    pub payload_offset: usize,
}

/// Parse the PES extension in the optional header data `ext`, which begins at the PES extension
/// flags, and return the `stream_id_extension`, if the extension has one.
fn parse_extended_stream_id(ext: &[u8]) -> Option<u8> {
    let flags = *ext.first()?;

    // The PES extension 2 flag (bit 0) must be set, and the pack header field flag (bit 6) clear
    // as its size is not known.
    if flags & 0x41 != 0x01 {
        return None;
    }

    // Skip the PES private data (16 bytes), the program packet sequence counter (2 bytes), and the
    // P-STD buffer (2 bytes).
    let skip = usize::from((flags >> 4) & 0xb);
    let skip = skip + (skip & 0x9);

    // The PES extension field length, and the stream ID extension flag followed by the value.
    let field = ext.get(1 + skip..1 + skip + 2)?;

    (field[0] & 0x7f > 0 && field[1] & 0x80 == 0).then_some(field[1])
}

impl PesHeader {
    /// The number of bytes of the payload, if the packet length is bounded.
    pub fn payload_len(&self) -> Option<usize> {
        if self.packet_len == 0 {
            None
        }
        else {
            (6 + self.packet_len).checked_sub(self.payload_offset)
        }
    }
}

/// Read a 33-bit timestamp from its 5 byte encoding. The `prefix` is the expected value of the 4
/// bits preceding the timestamp. Returns `None` if the marker bits are not valid.
fn read_timestamp(b: &[u8], prefix: u8) -> Option<u64> {
    let b = b.get(..5)?;

    if b[0] >> 4 != prefix || b[0] & 1 != 1 || b[2] & 1 != 1 || b[4] & 1 != 1 {
        return None;
    }

    Some(
        u64::from(b[0] >> 1 & 0x7) << 30
            | u64::from(b[1]) << 22
            | u64::from(b[2] >> 1) << 15
            | u64::from(b[3]) << 7
            | u64::from(b[4] >> 1),
    )
}

/// Parse a PES packet header. `buf` must begin with the `00 00 01` start code and the stream ID,
/// and should contain at least [`PES_MAX_HEADER_LEN`] bytes, or the entire packet. Returns `None`
/// if the buffer does not begin with a valid PES packet header.
///
/// Both the MPEG-2 PES header, and the MPEG-1 packet header, are supported.
pub fn parse_pes_header(buf: &[u8]) -> Option<PesHeader> {
    if buf.len() < 6 || buf[..3] != [0x00, 0x00, 0x01] {
        return None;
    }

    let stream_id = buf[3];

    // The stream IDs of packets (not system or pack headers).
    if stream_id < 0xb9 {
        return None;
    }

    let packet_len = usize::from(u16::from_be_bytes([buf[4], buf[5]]));

    if !stream_id_has_header(stream_id) {
        return Some(PesHeader {
            stream_id,
            packet_len,
            pts: None,
            dts: None,
            extended_stream_id: None,
            payload_offset: 6,
        });
    }

    let flags = *buf.get(6)?;

    if flags >> 6 == 0b10 {
        // MPEG-2 PES header.
        let pts_dts_flags = buf.get(7)? >> 6;
        let header_data_len = usize::from(*buf.get(8)?);
        let payload_offset = 9 + header_data_len;

        let (pts, dts) = match pts_dts_flags {
            0b10 => (Some(read_timestamp(buf.get(9..)?, 0b0010)?), None),
            0b11 => (
                Some(read_timestamp(buf.get(9..)?, 0b0011)?),
                Some(read_timestamp(buf.get(14..)?, 0b0001)?),
            ),
            0b01 => return None,
            _ => (None, None),
        };

        // The header data must be long enough for the timestamps.
        let ts_len = match pts_dts_flags {
            0b10 => 5,
            0b11 => 10,
            _ => 0,
        };

        if header_data_len < ts_len {
            return None;
        }

        if packet_len != 0 && packet_len + 6 < payload_offset {
            return None;
        }

        // The PES extension, if its flag is set, follows the timestamps. As in FFmpeg, the other
        // optional fields between them (ESCR, ES rate, DSM trick mode, additional copy info, and
        // CRC) are assumed absent.
        let extended_stream_id = if buf[7] & 0x01 != 0 {
            buf.get(9 + ts_len..payload_offset).and_then(parse_extended_stream_id)
        }
        else {
            None
        };

        return Some(PesHeader {
            stream_id,
            packet_len,
            pts,
            dts,
            extended_stream_id,
            payload_offset,
        });
    }

    // MPEG-1 packet header: stuffing bytes, then an optional STD buffer size, then optional
    // timestamps.
    let mut i = 6;

    while *buf.get(i)? == 0xff {
        i += 1;

        // Stuffing is at most 16 bytes.
        if i > 6 + 16 {
            return None;
        }
    }

    if buf.get(i)? >> 6 == 0b01 {
        i += 2;
    }

    let (pts, dts) = match buf.get(i)? >> 4 {
        0b0010 => {
            let pts = read_timestamp(buf.get(i..)?, 0b0010)?;
            i += 5;
            (Some(pts), None)
        }
        0b0011 => {
            let pts = read_timestamp(buf.get(i..)?, 0b0011)?;
            let dts = read_timestamp(buf.get(i + 5..)?, 0b0001)?;
            i += 10;
            (Some(pts), Some(dts))
        }
        _ => {
            if *buf.get(i)? != 0x0f {
                return None;
            }
            i += 1;
            (None, None)
        }
    };

    if packet_len != 0 && packet_len + 6 < i {
        return None;
    }

    Some(PesHeader { stream_id, packet_len, pts, dts, extended_stream_id: None, payload_offset: i })
}

/// Split a complete PES packet into the PES header and the payload. The payload is bounded by
/// the packet length, if the packet length is not unbounded (0).
pub fn split_pes(pes: &[u8]) -> Option<(PesHeader, &[u8])> {
    let hdr = parse_pes_header(pes)?;

    let end = if hdr.packet_len != 0 { (6 + hdr.packet_len).min(pes.len()) } else { pes.len() };
    let payload = pes.get(hdr.payload_offset..end)?;

    Some((hdr, payload))
}

/// Convert a time in milliseconds to the raw (33-bit) 90 kHz PTS.
pub fn ms_to_pts(ms: u32) -> u64 {
    (u64::from(ms) * (PTS_CLOCK_HZ / 1000)) % PTS_MODULUS
}

/// Returns the signed distance, `a - b`, in clock ticks, between two 33-bit timestamps, assuming
/// that they are less than half the range apart.
pub fn pts_delta(a: u64, b: u64) -> i64 {
    let d = (a.wrapping_sub(b)) % PTS_MODULUS;

    if d >= PTS_MODULUS / 2 { d as i64 - PTS_MODULUS as i64 } else { d as i64 }
}

/// Unwrap the 33-bit timestamp `raw` into the time in ticks since the timestamp `base`, choosing
/// the representation that is nearest to `hint`, an approximation of the time in ticks.
pub fn unwrap_pts(raw: u64, base: u64, hint: i64) -> i64 {
    let modulus = PTS_MODULUS as i64;
    let rel = ((raw % PTS_MODULUS) + PTS_MODULUS - (base % PTS_MODULUS)) as i64 % modulus;

    // The number of wraps that bring `rel` closest to `hint`.
    let k = hint.saturating_sub(rel).saturating_add(modulus / 2).div_euclid(modulus);

    rel.saturating_add(k.saturating_mul(modulus))
}

/// Encode a 33-bit timestamp in the 5 byte form, with the given 4 bit prefix.
pub fn write_timestamp(ts: u64, prefix: u8) -> [u8; 5] {
    let ts = ts % PTS_MODULUS;

    [
        prefix << 4 | ((ts >> 30) as u8 & 0x7) << 1 | 1,
        (ts >> 22) as u8,
        ((ts >> 15) as u8 & 0x7f) << 1 | 1,
        (ts >> 7) as u8,
        (ts as u8 & 0x7f) << 1 | 1,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_timestamp_round_trip() {
        for ts in [0, 1, 90_000, 0x1_2345_6789, PTS_MODULUS - 1] {
            assert_eq!(read_timestamp(&write_timestamp(ts, 0b0010), 0b0010), Some(ts));
        }

        // A marker bit is wrong.
        let mut b = write_timestamp(1234, 0b0010);
        b[2] &= !1;
        assert_eq!(read_timestamp(&b, 0b0010), None);
    }

    #[test]
    fn verify_mpeg2_pes_header() {
        let mut pes = vec![0, 0, 1, 0xc0, 0x00, 0x10, 0x80, 0x80, 0x05];
        pes.extend_from_slice(&write_timestamp(180_000, 0b0010));
        pes.extend_from_slice(&[1, 2, 3]);

        let hdr = parse_pes_header(&pes).unwrap();
        assert_eq!(hdr.stream_id, 0xc0);
        assert_eq!(hdr.pts, Some(180_000));
        assert_eq!(hdr.dts, None);
        assert_eq!(hdr.payload_offset, 14);
        assert_eq!(hdr.payload_len(), Some(8));
    }

    #[test]
    fn verify_stream_id_extension() {
        // The Blu-ray form: PTS, then the PES extension 2 with the stream ID extension 0x76.
        let mut pes = vec![0, 0, 1, 0xfd, 0x00, 0x20, 0x81, 0x81, 0x08];
        pes.extend_from_slice(&write_timestamp(90_000, 0b0010));
        pes.extend_from_slice(&[0x01, 0x81, 0x76, 1, 2, 3]);

        let hdr = parse_pes_header(&pes).unwrap();
        assert_eq!(hdr.extended_stream_id, Some(0x76));
        assert_eq!(hdr.pts, Some(90_000));
        assert_eq!(hdr.payload_offset, 17);

        // With the PES private data (16 bytes) and the P-STD buffer (2 bytes) before it.
        let mut pes = vec![0, 0, 1, 0xfd, 0x00, 0x40, 0x81, 0x81, 0x00];
        let mut ext = vec![0b1001_0001];
        ext.extend_from_slice(&[0xaa; 16]);
        ext.extend_from_slice(&[0x60, 0x00, 0x81, 0x72]);
        pes[8] = ext.len() as u8;
        pes.extend_from_slice(&ext);
        pes.extend_from_slice(&[0; 8]);
        // The PTS is not present: the flags byte is 0x01.
        pes[7] = 0x01;
        assert_eq!(parse_pes_header(&pes).unwrap().extended_stream_id, Some(0x72));

        // The stream ID extension flag is set, so there is no stream ID extension, or the
        // extension has the pack header field flag.
        let pes = [0, 0, 1, 0xfd, 0x00, 0x08, 0x81, 0x01, 0x03, 0x01, 0x81, 0xf6, 0, 0];
        assert_eq!(parse_pes_header(&pes).unwrap().extended_stream_id, None);
        let pes = [0, 0, 1, 0xfd, 0x00, 0x08, 0x81, 0x01, 0x03, 0x41, 0x81, 0x76, 0, 0];
        assert_eq!(parse_pes_header(&pes).unwrap().extended_stream_id, None);

        // No PES extension.
        let pes = [0, 0, 1, 0xc0, 0x00, 0x08, 0x81, 0x00, 0x00, 0, 0, 0];
        assert_eq!(parse_pes_header(&pes).unwrap().extended_stream_id, None);
    }

    #[test]
    fn verify_mpeg1_packet_header() {
        let mut pes = vec![0, 0, 1, 0xc0, 0x00, 0x10, 0xff, 0xff, 0x40, 0x20];
        pes.extend_from_slice(&write_timestamp(90_000, 0b0010));
        let hdr = parse_pes_header(&pes).unwrap();
        assert_eq!(hdr.pts, Some(90_000));
        assert_eq!(hdr.payload_offset, 15);

        // No timestamps.
        let pes = [0, 0, 1, 0xc0, 0x00, 0x10, 0xff, 0x0f, 0, 0];
        let hdr = parse_pes_header(&pes).unwrap();
        assert_eq!(hdr.pts, None);
        assert_eq!(hdr.payload_offset, 8);
    }

    #[test]
    fn verify_wrap_arithmetic() {
        assert_eq!(pts_delta(10, PTS_MODULUS - 10), 20);
        assert_eq!(pts_delta(PTS_MODULUS - 10, 10), -20);

        let base = PTS_MODULUS - 100;
        assert_eq!(unwrap_pts(50, base, 0), 150);
        assert_eq!(unwrap_pts(base + 7, base, 0), 7);
        // A second wrap, selected by the hint.
        assert_eq!(unwrap_pts(50, base, PTS_MODULUS as i64), PTS_MODULUS as i64 + 150);
    }
}
