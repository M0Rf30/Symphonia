// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ogg tag writing (Vorbis, Opus, FLAC-in-Ogg and Speex).
//!
//! The comment header of the first logical stream is rewritten. The header packets are re-paged
//! (the audio then starts on a fresh page), the page sequence numbers of the rest of that stream
//! are renumbered with their CRCs recomputed (granule positions are kept), and every page that
//! belongs to another logical stream is copied untouched. In a chained file only the first link
//! is rewritten.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::checksum::Crc32;
use symphonia_core::io::Monitor;
use symphonia_core::meta::{Tag, Visual};
use symphonia_format_ogg::writer::{
    FLAG_BOS, FLAG_EOS, NO_GRANULE, OGG_PAGE_HEADER_SIZE, OGG_PAGE_MARKER, build_page,
    packet_lacing,
};
use symphonia_metadata::embedded::flac::write::{
    BLOCK_TYPE_PADDING, BLOCK_TYPE_PICTURE, BLOCK_TYPE_VORBIS_COMMENT, block_header, picture_block,
};
use symphonia_metadata::embedded::vorbis::write::{
    CommentOptions, ExistingComments, comment_body_with, parse_existing,
};

use crate::flac::{comment_options, existing_or_default};
use crate::{Error, Result, WriteOptions, read_up_to, replace_file};

const VORBIS_COMMENT_PREFIX: &[u8] = b"\x03vorbis";
const OPUS_COMMENT_PREFIX: &[u8] = b"OpusTags";

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    let comment_opts = comment_options(opts);
    replace_file(path, |src, out| rewrite(src, out, tags, visuals, &comment_opts))
}

/// The codec of the logical stream being tagged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Vorbis,
    Opus,
    Flac,
    Speex,
}

impl Codec {
    /// Identifies the codec from the identification packet.
    fn detect(id: &[u8]) -> Result<Codec> {
        if id.starts_with(b"\x01vorbis") {
            Ok(Codec::Vorbis)
        }
        else if id.starts_with(b"OpusHead") {
            Ok(Codec::Opus)
        }
        else if id.starts_with(b"\x7fFLAC") && id.len() >= 13 {
            Ok(Codec::Flac)
        }
        else if id.starts_with(b"Speex   ") && id.len() >= 72 {
            Ok(Codec::Speex)
        }
        else {
            Err(Error::Unsupported(
                "the first logical Ogg stream is not Vorbis, Opus, FLAC or Speex".into(),
            ))
        }
    }

    /// Whether `packets`, the header packets following the identification packet, are all of the
    /// stream's header packets.
    fn headers_complete(self, id: &[u8], packets: &[Vec<u8>]) -> bool {
        match self {
            // Comment, setup.
            Codec::Vorbis => packets.len() >= 2,
            // Comment.
            Codec::Opus => !packets.is_empty(),
            // Comment, extra headers.
            Codec::Speex => {
                let extra = u32::from_le_bytes([id[68], id[69], id[70], id[71]]) as usize;
                packets.len() > extra
            }
            // Metadata blocks, up to the one flagged as the last (the packet count in the
            // identification packet is optional).
            Codec::Flac => {
                let count = usize::from(u16::from_be_bytes([id[7], id[8]]));

                packets.last().is_some_and(|p| p.first().is_some_and(|b| b & 0x80 != 0))
                    || (count > 0 && packets.len() >= count)
            }
        }
    }
}

/// An Ogg page, as read from the file.
struct Page {
    head: [u8; OGG_PAGE_HEADER_SIZE],
    lacing: Vec<u8>,
    body: Vec<u8>,
}

impl Page {
    fn flags(&self) -> u8 {
        self.head[5]
    }

    fn granule(&self) -> i64 {
        let h = &self.head;
        i64::from_le_bytes([h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13]])
    }

    fn serial(&self) -> u32 {
        let h = &self.head;
        u32::from_le_bytes([h[14], h[15], h[16], h[17]])
    }

    fn sequence(&self) -> u32 {
        let h = &self.head;
        u32::from_le_bytes([h[18], h[19], h[20], h[21]])
    }

    /// Writes the page as is.
    fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.head)?;
        out.write_all(&self.lacing)?;
        out.write_all(&self.body)
    }

    /// Writes the page with a different sequence number, and a recomputed CRC.
    fn write_with_sequence(&self, sequence: u32, out: &mut impl Write) -> io::Result<()> {
        let mut head = self.head;
        head[18..22].copy_from_slice(&sequence.to_le_bytes());
        head[22..26].copy_from_slice(&[0; 4]);

        let mut crc = Crc32::new(0);
        crc.process_buf_bytes(&head);
        crc.process_buf_bytes(&self.lacing);
        crc.process_buf_bytes(&self.body);

        head[22..26].copy_from_slice(&crc.crc().to_le_bytes());

        out.write_all(&head)?;
        out.write_all(&self.lacing)?;
        out.write_all(&self.body)
    }
}

enum PageRead {
    Page(Page),
    Eof,
    /// Bytes that are not (or not the whole of) a page.
    Junk(Vec<u8>),
}

fn read_page(input: &mut impl Read) -> Result<PageRead> {
    let mut head = [0u8; OGG_PAGE_HEADER_SIZE];

    let n = read_up_to(input, &mut head)?;

    if n == 0 {
        return Ok(PageRead::Eof);
    }

    if n < OGG_PAGE_HEADER_SIZE || head[..4] != OGG_PAGE_MARKER {
        return Ok(PageRead::Junk(head[..n].to_vec()));
    }

    let mut lacing = vec![0u8; usize::from(head[26])];

    if read_up_to(input, &mut lacing)? != lacing.len() {
        // Truncated file.
        let mut junk = head.to_vec();
        junk.extend_from_slice(&lacing);
        return Ok(PageRead::Junk(junk));
    }

    let body_len: usize = lacing.iter().map(|&l| usize::from(l)).sum();
    let mut body = vec![0u8; body_len];

    let n = read_up_to(input, &mut body)?;

    if n != body_len {
        let mut junk = head.to_vec();
        junk.extend_from_slice(&lacing);
        junk.extend_from_slice(&body[..n]);
        return Ok(PageRead::Junk(junk));
    }

    Ok(PageRead::Page(Page { head, lacing, body }))
}

/// The segments of the last header page that follow the last header packet.
struct Remainder {
    lacing: Vec<u8>,
    body: Vec<u8>,
    granule: i64,
}

/// The header packets of the stream being tagged, and where they end.
struct Headers {
    codec: Codec,
    serial: u32,
    /// The identification packet.
    id: Vec<u8>,
    /// The sequence number of the page that holds the identification packet.
    id_sequence: u32,
    /// The header packets after the identification packet.
    packets: Vec<Vec<u8>>,
    /// The index, among the pages of the stream, of the page the last header packet ends on.
    last_page: usize,
    /// Segments on that page that belong to packets after the headers.
    remainder: Option<Remainder>,
    /// Whether that page is flagged as the end of the stream.
    eos: bool,
}

/// Reads the pages of the first logical stream up to the end of its header packets.
fn gather_headers(input: &mut impl Read) -> Result<Headers> {
    let first = match read_page(input)? {
        PageRead::Page(page) => page,
        _ => return Err(Error::Malformed("the file does not start with an Ogg page".into())),
    };

    if first.flags() & FLAG_BOS == 0 {
        return Err(Error::Malformed("the first Ogg page is not a beginning of stream".into()));
    }

    let ends_packet = first.lacing.iter().filter(|&&l| l < 255).count();

    if ends_packet != 1 || first.lacing.last() == Some(&255) {
        return Err(Error::Unsupported(
            "the first Ogg page holds more than the identification packet".into(),
        ));
    }

    let serial = first.serial();
    let id_sequence = first.sequence();
    let codec = Codec::detect(&first.body)?;

    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut partial = Vec::new();
    let mut index = 0;

    loop {
        let page = match read_page(input)? {
            PageRead::Page(page) => page,
            _ => return Err(Error::Malformed("the header packets are incomplete".into())),
        };

        if page.serial() != serial {
            continue;
        }

        index += 1;

        let mut offset = 0;

        for (i, &seg) in page.lacing.iter().enumerate() {
            let seg = usize::from(seg);

            partial.extend_from_slice(&page.body[offset..offset + seg]);
            offset += seg;

            if seg == 255 {
                continue;
            }

            packets.push(std::mem::take(&mut partial));

            if codec.headers_complete(&first.body, &packets) {
                let rest = &page.lacing[i + 1..];

                let remainder = (!rest.is_empty()).then(|| Remainder {
                    lacing: rest.to_vec(),
                    body: page.body[offset..].to_vec(),
                    granule: if rest.iter().any(|&l| l < 255) {
                        page.granule()
                    }
                    else {
                        NO_GRANULE
                    },
                });

                return Ok(Headers {
                    codec,
                    serial,
                    id: first.body,
                    id_sequence,
                    packets,
                    last_page: index,
                    remainder,
                    eos: page.flags() & FLAG_EOS != 0,
                });
            }
        }
    }
}

/// Builds the new identification packet and header packets.
fn build_headers(
    headers: &Headers,
    tags: &[Tag],
    visuals: &[Visual],
    comment_opts: &CommentOptions,
) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let malformed = |msg: &str| Error::Malformed(msg.to_string());

    // Parses the existing comment header, which is the first header packet. `prefix` is what
    // precedes the Vorbis comment in the packet.
    let existing = |prefix: &[u8]| -> Result<ExistingComments> {
        let comment = headers.packets.first().ok_or_else(|| malformed("missing comment header"))?;

        let body = comment
            .strip_prefix(prefix)
            .ok_or_else(|| malformed("the comment header has an unexpected signature"))?;

        // A comment header that cannot be parsed is replaced.
        Ok(existing_or_default(parse_existing(body)))
    };

    match headers.codec {
        Codec::Vorbis => {
            let ExistingComments { vendor, preserved, .. } = existing(VORBIS_COMMENT_PREFIX)?;

            let mut comment = VORBIS_COMMENT_PREFIX.to_vec();
            comment.extend(comment_body_with(&vendor, tags, visuals, &preserved, comment_opts)?);
            // The framing bit.
            comment.push(1);

            // The setup header.
            Ok((headers.id.clone(), vec![comment, headers.packets[1].clone()]))
        }
        Codec::Opus => {
            let ExistingComments { vendor, preserved, trailing } = existing(OPUS_COMMENT_PREFIX)?;

            let mut comment = OPUS_COMMENT_PREFIX.to_vec();
            comment.extend(comment_body_with(&vendor, tags, visuals, &preserved, comment_opts)?);
            // Opus tools may append binary padding to the comment header, keep it.
            comment.extend(trailing);

            Ok((headers.id.clone(), vec![comment]))
        }
        Codec::Speex => {
            let ExistingComments { vendor, preserved, .. } = existing(&[])?;

            let comment = comment_body_with(&vendor, tags, visuals, &preserved, comment_opts)?;

            let mut packets = vec![comment];
            // The extra headers.
            packets.extend(headers.packets[1..].iter().cloned());

            Ok((headers.id.clone(), packets))
        }
        Codec::Flac => {
            // Every packet is a metadata block, with its header.
            let mut blocks: Vec<(u8, Vec<u8>)> = Vec::new();
            let mut vorbis_comment = None;

            for packet in &headers.packets {
                if packet.len() < 4 {
                    return Err(malformed("truncated FLAC metadata block"));
                }

                let kind = packet[0] & 0x7f;

                match kind {
                    BLOCK_TYPE_VORBIS_COMMENT => {
                        vorbis_comment.get_or_insert(&packet[4..]);
                    }
                    BLOCK_TYPE_PICTURE | BLOCK_TYPE_PADDING => (),
                    _ => blocks.push((kind, packet[4..].to_vec())),
                }
            }

            let ExistingComments { vendor, preserved, .. } =
                existing_or_default(vorbis_comment.and_then(parse_existing));

            blocks.push((
                BLOCK_TYPE_VORBIS_COMMENT,
                comment_body_with(&vendor, tags, &[], &preserved, comment_opts)?,
            ));

            for visual in visuals {
                blocks.push((BLOCK_TYPE_PICTURE, picture_block(visual)?));
            }

            let count = u16::try_from(blocks.len())
                .map_err(|_| Error::Unsupported("too many FLAC metadata blocks".into()))?;

            let last = blocks.len() - 1;

            let mut packets = Vec::with_capacity(blocks.len());

            for (i, (kind, body)) in blocks.into_iter().enumerate() {
                let mut packet = block_header(i == last, kind, body.len())?.to_vec();
                packet.extend(body);
                packets.push(packet);
            }

            // Update the number of header packets in the identification packet.
            let mut id = headers.id.clone();
            id[7..9].copy_from_slice(&count.to_be_bytes());

            Ok((id, packets))
        }
    }
}

/// Packs packets into Ogg pages.
struct Paginator {
    serial: u32,
    sequence: u32,
    pages: Vec<Vec<u8>>,
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// Whether the pending page continues a packet of the previous page.
    continued: bool,
    /// Whether a packet ends on the pending page.
    completed: bool,
}

impl Paginator {
    fn new(serial: u32, sequence: u32) -> Self {
        Paginator {
            serial,
            sequence,
            pages: Vec::new(),
            lacing: Vec::with_capacity(255),
            body: Vec::new(),
            continued: false,
            completed: false,
        }
    }

    fn push_packet(&mut self, packet: &[u8]) {
        let mut offset = 0;
        let mut first = true;

        loop {
            if self.lacing.len() == 255 {
                // The packet carries on in the next page, unless nothing of it was written.
                self.flush(!first, false);
            }

            let seg = (packet.len() - offset).min(255);

            self.lacing.push(seg as u8);
            self.body.extend_from_slice(&packet[offset..offset + seg]);
            offset += seg;
            first = false;

            if seg < 255 {
                self.completed = true;
                return;
            }
        }
    }

    fn flush(&mut self, end_continues: bool, eos: bool) {
        // Header pages have a granule position of 0.
        let granule = if self.completed { 0 } else { NO_GRANULE };

        let flags = u8::from(self.continued) | if eos { FLAG_EOS } else { 0 };

        self.pages.push(build_page(
            flags,
            granule,
            self.serial,
            self.sequence,
            &self.lacing,
            &self.body,
        ));

        self.sequence += 1;
        self.continued = end_continues;
        self.completed = false;
        self.lacing.clear();
        self.body.clear();
    }

    /// Flushes the pending page and returns the pages.
    fn finish(mut self, eos: bool) -> Vec<Vec<u8>> {
        if !self.lacing.is_empty() {
            self.flush(false, eos);
        }

        self.pages
    }
}

fn rewrite(
    src: &mut File,
    out: &mut BufWriter<File>,
    tags: &[Tag],
    visuals: &[Visual],
    comment_opts: &CommentOptions,
) -> Result<()> {
    let headers = gather_headers(&mut BufReader::with_capacity(1 << 16, &mut *src))?;

    let (id, packets) = build_headers(&headers, tags, visuals, comment_opts)?;

    src.seek(SeekFrom::Start(0))?;

    let mut input = BufReader::with_capacity(1 << 16, &mut *src);

    let mut stream_page = 0;
    let mut sequence = 0u32;

    loop {
        let page = match read_page(&mut input)? {
            PageRead::Page(page) => page,
            PageRead::Eof => break,
            PageRead::Junk(junk) => {
                // Trailing data that is not a page.
                out.write_all(&junk)?;
                io::copy(&mut input, out)?;
                break;
            }
        };

        if page.serial() != headers.serial {
            page.write_to(out)?;
            continue;
        }

        let index = stream_page;
        stream_page += 1;

        if index == 0 {
            if id == headers.id {
                page.write_to(out)?;
            }
            else {
                // The identification packet changed (the header count of a FLAC stream).
                let lacing = packet_lacing(id.len());
                out.write_all(&build_page(
                    page.flags(),
                    page.granule(),
                    headers.serial,
                    page.sequence(),
                    &lacing,
                    &id,
                ))?;
            }

            sequence = headers.id_sequence.wrapping_add(1);
        }
        else if index < headers.last_page {
            // An old header page.
        }
        else if index == headers.last_page {
            let mut paginator = Paginator::new(headers.serial, sequence);

            for packet in &packets {
                paginator.push_packet(packet);
            }

            let pages = paginator.finish(headers.eos && headers.remainder.is_none());

            for page in &pages {
                out.write_all(page)?;
            }

            sequence = sequence.wrapping_add(pages.len() as u32);

            if let Some(rest) = &headers.remainder {
                // Data packets that shared a page with the last header packet get a page of
                // their own.
                let flags = if headers.eos { FLAG_EOS } else { 0 };

                out.write_all(&build_page(
                    flags,
                    rest.granule,
                    headers.serial,
                    sequence,
                    &rest.lacing,
                    &rest.body,
                ))?;

                sequence = sequence.wrapping_add(1);
            }

            if headers.eos {
                io::copy(&mut input, out)?;
                break;
            }
        }
        else {
            page.write_with_sequence(sequence, out)?;
            sequence = sequence.wrapping_add(1);

            if page.flags() & FLAG_EOS != 0 {
                // The end of the first link: the rest of the file is not touched.
                io::copy(&mut input, out)?;
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use symphonia_format_ogg::writer::page_crc;

    use super::*;

    /// Splits pages back into packets, checking the page structure.
    fn unpack(pages: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut partial = Vec::new();
        let mut continues = false;

        for (i, page) in pages.iter().enumerate() {
            assert_eq!(&page[..4], b"OggS");
            assert_eq!(u32::from_le_bytes([page[18], page[19], page[20], page[21]]), 7 + i as u32);
            assert_eq!(page[5] & 1 != 0, continues, "continuation flag of page {i}");

            let mut zeroed = page.clone();
            zeroed[22..26].copy_from_slice(&[0; 4]);
            let crc = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
            assert_eq!(page_crc(&zeroed), crc);

            let nseg = usize::from(page[26]);
            let mut offset = 27 + nseg;
            let mut completed = false;

            for &seg in &page[27..27 + nseg] {
                partial.extend_from_slice(&page[offset..offset + usize::from(seg)]);
                offset += usize::from(seg);

                continues = seg == 255;

                if seg < 255 {
                    packets.push(std::mem::take(&mut partial));
                    completed = true;
                }
            }

            assert_eq!(offset, page.len());

            let granule = i64::from_le_bytes(page[6..14].try_into().expect("8 bytes"));
            assert_eq!(granule, if completed { 0 } else { NO_GRANULE });
        }

        assert!(!continues);
        packets
    }

    #[test]
    fn verify_pagination_of_awkward_packet_sizes() {
        let sizes = [0usize, 1, 254, 255, 256, 509, 510, 511, 255 * 255, 255 * 255 + 1, 70_000, 3];

        let packets: Vec<Vec<u8>> =
            sizes.iter().map(|&n| (0..n).map(|i| (i % 251) as u8).collect()).collect();

        let mut paginator = Paginator::new(0x1234, 7);
        packets.iter().for_each(|p| paginator.push_packet(p));

        let pages = paginator.finish(true);

        assert!(pages.len() > 1);
        assert_ne!(pages.last().unwrap()[5] & FLAG_EOS, 0);
        assert_eq!(unpack(&pages), packets);
    }

    #[test]
    fn verify_packet_lacing() {
        assert_eq!(packet_lacing(0), vec![0]);
        assert_eq!(packet_lacing(51), vec![51]);
        assert_eq!(packet_lacing(255), vec![255, 0]);
        assert_eq!(packet_lacing(256), vec![255, 1]);
    }
}
