// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A minimal Ogg page **writer** (RFC 3533), behind the `writer` cargo feature.
//!
//! [`OggPacketWriter`] packs packets into pages: it builds the segment (lacing) table, splits
//! packets that do not fit into 255 segments across pages (continuation flag), maintains the
//! page sequence number and the beginning/end-of-stream flags, stores the caller supplied
//! granule position and computes the page CRC. It is codec agnostic; the codec decides what a
//! granule position means (for Opus, RFC 7845: the number of 48 kHz samples that can be decoded
//! up to the end of the last packet completed on the page, including pre-skip).
//!
//! This is deliberately small: one logical stream per writer (multiplex by using several
//! writers on a shared sink), no packet-boundary optimisation beyond a size target for pages.

use std::io::{self, Write};

use symphonia_core::checksum::Crc32;
use symphonia_core::io::Monitor;

const OGG_PAGE_MARKER: [u8; 4] = *b"OggS";
const OGG_PAGE_HEADER_SIZE: usize = 27;
const FLAG_CONTINUED: u8 = 0x01;
const FLAG_BOS: u8 = 0x02;
const FLAG_EOS: u8 = 0x04;
/// Granule position of a page on which no packet ends (RFC 3533 section 6).
pub const NO_GRANULE: i64 = -1;

/// Computes the Ogg page checksum: CRC-32 with polynomial `0x04c11db7`, initial value 0, no bit
/// reflection and no final XOR, over the whole page with the checksum field set to zero.
pub fn page_crc(data: &[u8]) -> u32 {
    let mut crc = Crc32::new(0);
    crc.process_buf_bytes(data);
    crc.crc()
}

/// Serialises one complete Ogg page, including its CRC.
///
/// `lacing` is the segment table (at most 255 entries) and `body` the concatenation of the
/// segments, so `body.len()` must equal the sum of `lacing`.
pub fn build_page(
    header_type: u8,
    granule: i64,
    serial: u32,
    sequence: u32,
    lacing: &[u8],
    body: &[u8],
) -> Vec<u8> {
    assert!(lacing.len() <= 255, "an Ogg page has at most 255 segments");
    debug_assert_eq!(lacing.iter().map(|&l| l as usize).sum::<usize>(), body.len());
    let mut page = Vec::with_capacity(OGG_PAGE_HEADER_SIZE + lacing.len() + body.len());
    page.extend_from_slice(&OGG_PAGE_MARKER);
    page.push(0); // stream structure version
    page.push(header_type);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&[0; 4]); // checksum placeholder
    page.push(lacing.len() as u8);
    page.extend_from_slice(lacing);
    page.extend_from_slice(body);
    let crc = page_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Writes the packets of one logical Ogg bitstream to a [`Write`] sink.
pub struct OggPacketWriter<W: Write> {
    inner: W,
    serial: u32,
    sequence: u32,
    page_target: usize,
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// Granule position of the last packet completed on the pending page.
    page_granule: i64,
    /// Whether the pending page starts with the continuation of the previous page's packet.
    continued: bool,
    began: bool,
    ended: bool,
    last_granule: i64,
}

impl<W: Write> OggPacketWriter<W> {
    /// Creates a writer for the logical bitstream identified by `serial`.
    pub fn new(inner: W, serial: u32) -> Self {
        OggPacketWriter {
            inner,
            serial,
            sequence: 0,
            page_target: 4096,
            lacing: Vec::with_capacity(255),
            body: Vec::new(),
            page_granule: NO_GRANULE,
            continued: false,
            began: false,
            ended: false,
            last_granule: 0,
        }
    }

    /// Sets the body size (bytes) at which a page is emitted once a packet completes. Smaller
    /// pages lower the streaming latency, larger ones reduce the container overhead.
    /// Default 4096.
    pub fn set_page_target(&mut self, bytes: usize) {
        self.page_target = bytes.max(1);
    }

    /// The page sequence number the next emitted page will carry.
    pub fn next_sequence(&self) -> u32 {
        self.sequence
    }

    /// Appends one packet. `granule` is the granule position *after* this packet; it is stored
    /// on the page on which the packet ends. If `end_of_stream` is set, the page is flushed with
    /// the end-of-stream flag and no further packets are accepted.
    pub fn write_packet(
        &mut self,
        data: &[u8],
        granule: i64,
        end_of_stream: bool,
    ) -> io::Result<()> {
        if self.ended {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "ogg stream already ended"));
        }
        // A packet is laced into segments of 255 bytes, terminated by a segment < 255 (possibly
        // of length 0 when the packet size is a multiple of 255).
        let mut off = 0usize;
        loop {
            if self.lacing.len() == 255 {
                // The packet does not fit: continue it on the next page.
                self.emit_page(true, false)?;
            }
            let seg = (data.len() - off).min(255);
            self.lacing.push(seg as u8);
            self.body.extend_from_slice(&data[off..off + seg]);
            off += seg;
            if seg < 255 {
                break;
            }
        }
        self.page_granule = granule;
        self.last_granule = granule;
        if end_of_stream {
            self.emit_page(false, true)
        }
        else if self.lacing.len() == 255 || self.body.len() >= self.page_target {
            self.emit_page(false, false)
        }
        else {
            Ok(())
        }
    }

    /// Forces the pending packets onto a page now (e.g. to give each Opus header packet its own
    /// page as RFC 7845 requires). Does nothing if no packet is pending.
    pub fn flush_page(&mut self) -> io::Result<()> {
        if self.lacing.is_empty() {
            return Ok(());
        }
        self.emit_page(false, false)
    }

    /// Flushes the pending page, marking it end-of-stream, and returns the sink. If everything
    /// was already written without an end-of-stream flag, an empty end-of-stream page carrying
    /// the last granule position is appended.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.ended {
            self.page_granule =
                if self.lacing.is_empty() { self.last_granule } else { self.page_granule };
            self.emit_page(false, true)?;
        }
        self.inner.flush()?;
        Ok(self.inner)
    }

    /// Gives back the sink without flushing pending packets.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Emits the pending page. `packet_continues` tells that the last packet on it is
    /// incomplete (it carries on in the next page).
    fn emit_page(&mut self, packet_continues: bool, end_of_stream: bool) -> io::Result<()> {
        let mut flags = 0u8;
        if self.continued {
            flags |= FLAG_CONTINUED;
        }
        if !self.began {
            flags |= FLAG_BOS;
        }
        if end_of_stream {
            flags |= FLAG_EOS;
        }
        // `page_granule` is `NO_GRANULE` unless a packet was completed on this page.
        let page = build_page(
            flags,
            self.page_granule,
            self.serial,
            self.sequence,
            &self.lacing,
            &self.body,
        );
        self.inner.write_all(&page)?;
        self.sequence += 1;
        self.began = true;
        self.ended = end_of_stream;
        self.continued = packet_continues;
        self.lacing.clear();
        self.body.clear();
        // The granule position of a page that ends mid-packet belongs to the packets that were
        // completed on it; the next page starts without one until a packet completes.
        self.page_granule = NO_GRANULE;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC of the ASCII digits "123456789" with the Ogg CRC (poly 0x04c11db7, init 0, no
    /// reflection, no xorout; i.e. CRC-32/MPEG-2 with a zero seed). The value was computed with
    /// an independent bit-by-bit implementation.
    #[test]
    fn crc_check_value() {
        assert_eq!(page_crc(b"123456789"), 0x89A1897F);
        assert_eq!(page_crc(&[]), 0);
        assert_eq!(page_crc(&[0]), 0);
        assert_eq!(page_crc(&[1]), 0x04C11DB7);
    }

    /// The first page of a real stream written by libogg/libopusenc (an `OpusHead` page, 47
    /// bytes). Rebuilding it from its decoded fields must reproduce every byte, CRC included.
    #[test]
    fn matches_libogg_head_page() {
        const REFERENCE: [u8; 47] = [
            0x4f, 0x67, 0x67, 0x53, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x95, 0x45, 0xe9, 0x12, 0x00, 0x00, 0x00, 0x00, 0x46, 0x22, 0x19, 0x8a, 0x01, 0x13,
            0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 0x01, 0x02, 0x38, 0x01, 0x80, 0xbb,
            0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(REFERENCE.len(), 27 + 1 + 19);
        let head = &REFERENCE[28..];
        assert_eq!(&head[..8], b"OpusHead");
        let serial = u32::from_le_bytes(REFERENCE[14..18].try_into().unwrap());
        assert_eq!(serial, 0x12E94595);
        let page = build_page(FLAG_BOS, 0, serial, 0, &[19], head);
        assert_eq!(&page[..], &REFERENCE[..]);
        // The same page through the packet writer.
        let mut w = OggPacketWriter::new(Vec::new(), serial);
        w.write_packet(head, 0, false).unwrap();
        w.flush_page().unwrap();
        assert_eq!(w.into_inner(), REFERENCE.to_vec());
    }

    fn parse_pages(mut data: &[u8]) -> Vec<(u8, i64, u32, u32, Vec<u8>, Vec<u8>)> {
        let mut pages = Vec::new();
        while !data.is_empty() {
            assert_eq!(&data[..4], b"OggS");
            let n = data[26] as usize;
            let lacing = data[27..27 + n].to_vec();
            let body_len: usize = lacing.iter().map(|&l| l as usize).sum();
            let end = 27 + n + body_len;
            let mut zeroed = data[..end].to_vec();
            let crc = u32::from_le_bytes(zeroed[22..26].try_into().unwrap());
            zeroed[22..26].fill(0);
            assert_eq!(page_crc(&zeroed), crc, "page CRC");
            pages.push((
                data[5],
                i64::from_le_bytes(data[6..14].try_into().unwrap()),
                u32::from_le_bytes(data[14..18].try_into().unwrap()),
                u32::from_le_bytes(data[18..22].try_into().unwrap()),
                lacing,
                data[27 + n..end].to_vec(),
            ));
            data = &data[end..];
        }
        pages
    }

    #[test]
    fn packets_pages_flags_and_granules() {
        let mut w = OggPacketWriter::new(Vec::new(), 7);
        w.write_packet(b"head", 0, false).unwrap();
        w.flush_page().unwrap();
        w.write_packet(&[1u8; 10], 960, false).unwrap();
        w.write_packet(&[2u8; 20], 1920, false).unwrap();
        w.write_packet(&[3u8; 5], 2880, true).unwrap();
        let out = w.finish().unwrap();
        let pages = parse_pages(&out);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].0, FLAG_BOS);
        assert_eq!(pages[0].1, 0);
        assert_eq!(pages[0].3, 0);
        assert_eq!(pages[1].0, FLAG_EOS);
        assert_eq!(pages[1].1, 2880);
        assert_eq!(pages[1].3, 1);
        assert_eq!(pages[1].4, vec![10, 20, 5]);
        assert!(pages.iter().all(|p| p.2 == 7));
    }

    #[test]
    fn long_packets_span_pages_with_continuation() {
        let mut w = OggPacketWriter::new(Vec::new(), 1);
        // 70_000 bytes needs 275 segments: 255 on the first page, 20 on the second.
        let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        w.write_packet(&big, 1234, false).unwrap();
        // A packet whose size is a multiple of 255 ends with a zero-length segment.
        let exact = vec![9u8; 510];
        w.write_packet(&exact, 2468, true).unwrap();
        let out = w.finish().unwrap();
        let pages = parse_pages(&out);
        assert_eq!(pages[0].4.len(), 255);
        assert_eq!(pages[0].1, NO_GRANULE, "no packet ends on the first page");
        assert_eq!(pages[0].0 & FLAG_CONTINUED, 0);
        assert_eq!(pages[1].0 & FLAG_CONTINUED, FLAG_CONTINUED);
        // Reassemble through the lacing values.
        let mut packets: Vec<Vec<u8>> = vec![];
        let mut cur = vec![];
        for p in &pages {
            let mut off = 0;
            for &l in &p.4 {
                cur.extend_from_slice(&p.5[off..off + l as usize]);
                off += l as usize;
                if l < 255 {
                    packets.push(std::mem::take(&mut cur));
                }
            }
        }
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], big);
        assert_eq!(packets[1], exact);
        let last = pages.last().unwrap();
        assert_eq!(last.0 & FLAG_EOS, FLAG_EOS);
        assert_eq!(last.1, 2468);
        // Sequence numbers are consecutive.
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(p.3 as usize, i);
        }
    }

    #[test]
    fn finish_without_eos_adds_empty_eos_page() {
        let mut w = OggPacketWriter::new(Vec::new(), 3);
        w.set_page_target(1);
        w.write_packet(b"abc", 48, false).unwrap(); // emitted immediately (target reached)
        let out = w.finish().unwrap();
        let pages = parse_pages(&out);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1].0, FLAG_EOS);
        assert_eq!(pages[1].1, 48);
        assert!(pages[1].4.is_empty());
    }

    #[test]
    fn rejects_writes_after_eos() {
        let mut w = OggPacketWriter::new(Vec::new(), 3);
        w.write_packet(b"x", 1, true).unwrap();
        assert!(w.write_packet(b"y", 2, false).is_err());
    }
}
