// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Detection of tags that are appended after the audio data of a MPEG audio stream.
//!
//! The audio data of a MPEG audio stream may be followed by an ID3v1 tag, a Lyrics3 (v1 or v2)
//! tag, an APEv2 tag, or an ID3v2 tag with a footer (ID3v2.4), in any order. None of these contain
//! audio frames, but they may contain data that resembles frame headers, and their sizes must not
//! be included in the duration estimate of the stream.

use std::io::{Seek, SeekFrom};

use symphonia_core::io::{MediaSourceStream, ReadBytes};

/// The size of an ID3v1 tag.
const ID3V1_LEN: u64 = 128;
/// The size of an ID3v1 extended tag ("TAG+") that precedes the ID3v1 tag.
const ID3V1_EXT_LEN: u64 = 227;
/// The size of the ID3v2 header or footer.
const ID3V2_HEADER_LEN: u64 = 10;
/// The size of an APEv2 header or footer.
const APEV2_HEADER_LEN: u64 = 32;
/// The size of the Lyrics3v2 trailer (6 digits of size, then "LYRICS200").
const LYRICS3V2_TRAILER_LEN: u64 = 15;
/// The maximum size of a Lyrics3v1 tag.
const LYRICS3V1_MAX_LEN: u64 = 5100 + 11 + 9;

/// Read `buf.len()` bytes at the absolute position `pos`.
fn read_at(mss: &mut MediaSourceStream<'_>, pos: u64, buf: &mut [u8]) -> Option<()> {
    mss.seek(SeekFrom::Start(pos)).ok()?;
    mss.read_buf_exact(buf).ok()
}

/// Given that audio data begins at `start` and the stream is `len` bytes long, find the position
/// where the audio data ends by skipping any tags appended after it.
///
/// The stream must be seekable. The position of the stream is not restored.
fn find_audio_end_inner(mss: &mut MediaSourceStream<'_>, start: u64, len: u64) -> u64 {
    let mut end = len;

    // Bound the number of tags that will be skipped.
    for _ in 0..16 {
        let avail = end.saturating_sub(start);

        // ID3v1 (and ID3v1 extended).
        if avail >= ID3V1_LEN {
            let mut id = [0; 3];

            if read_at(mss, end - ID3V1_LEN, &mut id) == Some(()) && &id == b"TAG" {
                end -= ID3V1_LEN;

                // An extended tag directly precedes the ID3v1 tag.
                if end.saturating_sub(start) >= ID3V1_EXT_LEN {
                    let mut id = [0; 4];

                    if read_at(mss, end - ID3V1_EXT_LEN, &mut id) == Some(()) && &id == b"TAG+" {
                        end -= ID3V1_EXT_LEN;
                    }
                }

                continue;
            }
        }

        // Lyrics3v2 and Lyrics3v1.
        if avail >= LYRICS3V2_TRAILER_LEN {
            let mut trailer = [0; LYRICS3V2_TRAILER_LEN as usize];

            if read_at(mss, end - LYRICS3V2_TRAILER_LEN, &mut trailer) == Some(()) {
                if &trailer[6..] == b"LYRICS200" {
                    // The size field is 6 ASCII digits, and excludes the trailer itself.
                    let size = std::str::from_utf8(&trailer[..6])
                        .ok()
                        .and_then(|digits| digits.parse::<u64>().ok());

                    if let Some(size) = size {
                        let total = size + LYRICS3V2_TRAILER_LEN;

                        if total <= avail {
                            let mut id = [0; 11];

                            if read_at(mss, end - total, &mut id) == Some(())
                                && &id == b"LYRICSBEGIN"
                            {
                                end -= total;
                                continue;
                            }
                        }
                    }
                }
                else if &trailer[6..] == b"LYRICSEND" {
                    // Lyrics3v1 has no size field. Search backwards for the start of the tag.
                    let search_len = avail.min(LYRICS3V1_MAX_LEN);
                    let mut buf = vec![0; search_len as usize];

                    if read_at(mss, end - search_len, &mut buf) == Some(()) {
                        let begin = b"LYRICSBEGIN";

                        if let Some(idx) = buf.windows(begin.len()).rposition(|w| w == begin) {
                            end -= search_len - idx as u64;
                            continue;
                        }
                    }
                }
            }
        }

        // APEv2 footer.
        if avail >= APEV2_HEADER_LEN {
            let mut footer = [0; APEV2_HEADER_LEN as usize];

            if read_at(mss, end - APEV2_HEADER_LEN, &mut footer) == Some(())
                && &footer[..8] == b"APETAGEX"
            {
                // The size includes the footer and the items, but not the header.
                let size =
                    u64::from(u32::from_le_bytes([footer[12], footer[13], footer[14], footer[15]]));
                let flags = u32::from_le_bytes([footer[20], footer[21], footer[22], footer[23]]);

                // Bit 31 indicates the tag contains a header. Bit 29 would indicate this is the
                // header itself, which is not valid for a footer.
                let has_header = flags & (1 << 31) != 0;
                let is_header = flags & (1 << 29) != 0;

                let total = size + if has_header { APEV2_HEADER_LEN } else { 0 };

                if !is_header && size >= APEV2_HEADER_LEN && total <= avail {
                    end -= total;
                    continue;
                }
            }
        }

        // ID3v2 footer ("3DI"), present in some ID3v2.4 tags appended to the stream.
        if avail >= 2 * ID3V2_HEADER_LEN {
            let mut footer = [0; ID3V2_HEADER_LEN as usize];

            if read_at(mss, end - ID3V2_HEADER_LEN, &mut footer) == Some(())
                && &footer[..3] == b"3DI"
                && footer[6..10].iter().all(|&b| b < 0x80)
            {
                let size = footer[6..10].iter().fold(0u64, |acc, &b| (acc << 7) | u64::from(b));

                // The tag consists of the header, the frames and padding, and the footer.
                let total = ID3V2_HEADER_LEN + size + ID3V2_HEADER_LEN;

                if total <= avail {
                    let mut id = [0; 3];

                    if read_at(mss, end - total, &mut id) == Some(()) && &id == b"ID3" {
                        end -= total;
                        continue;
                    }
                }
            }
        }

        // No more trailing tags.
        break;
    }

    end
}

/// Given that audio data begins at `start` and the stream is `len` bytes long, find the position
/// where the audio data ends by skipping any tags appended after it.
///
/// Tags that are recognized are ID3v1 (with the extended "TAG+" tag), Lyrics3v1 and v2, APEv2, and
/// ID3v2 with a footer. The stream must be seekable. The position of the stream is restored
/// before returning.
pub fn find_audio_end(mss: &mut MediaSourceStream<'_>, start: u64, len: u64) -> u64 {
    // Save the current position, and restore it after.
    let pos = mss.pos();

    let end = find_audio_end_inner(mss, start, len);

    // Restoring the position can only fail if the stream was seekable and became unseekable,
    // in which case, there's nothing that can be done.
    let _ = mss.seek(SeekFrom::Start(pos));

    end
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::io::MediaSourceStream;

    use super::*;

    fn end_of(data: Vec<u8>, start: u64) -> u64 {
        let len = data.len() as u64;
        let mut mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        find_audio_end(&mut mss, start, len)
    }

    fn id3v1() -> Vec<u8> {
        let mut tag = vec![0; 128];
        tag[..3].copy_from_slice(b"TAG");
        tag
    }

    fn apev2(with_header: bool) -> Vec<u8> {
        // Two items of 16 bytes each.
        let items = [7u8; 32];
        let size = (items.len() + 32) as u32;
        let flags: u32 = if with_header { 1 << 31 } else { 0 };

        let mut footer = Vec::new();
        footer.extend_from_slice(b"APETAGEX");
        footer.extend_from_slice(&2000u32.to_le_bytes());
        footer.extend_from_slice(&size.to_le_bytes());
        footer.extend_from_slice(&2u32.to_le_bytes());
        footer.extend_from_slice(&flags.to_le_bytes());
        footer.extend_from_slice(&[0; 8]);

        let mut tag = Vec::new();

        if with_header {
            let mut header = footer.clone();
            header[20..24].copy_from_slice(&((1u32 << 31) | (1 << 29) | (1 << 30)).to_le_bytes());
            tag.extend_from_slice(&header);
        }

        tag.extend_from_slice(&items);
        tag.extend_from_slice(&footer);
        tag
    }

    #[test]
    fn verify_no_trailing_tags() {
        assert_eq!(end_of(vec![0xaa; 1000], 0), 1000);
    }

    #[test]
    fn verify_id3v1_is_skipped() {
        let mut data = vec![0xaa; 1000];
        data.extend(id3v1());
        assert_eq!(end_of(data, 0), 1000);
    }

    #[test]
    fn verify_apev2_is_skipped() {
        for with_header in [false, true] {
            let mut data = vec![0xaa; 1000];
            data.extend(apev2(with_header));
            assert_eq!(end_of(data, 0), 1000);
        }
    }

    #[test]
    fn verify_apev2_and_id3v1_are_skipped() {
        let mut data = vec![0xaa; 1000];
        data.extend(apev2(true));
        data.extend(id3v1());
        assert_eq!(end_of(data, 0), 1000);
    }

    #[test]
    fn verify_lyrics3v2_and_id3v1_are_skipped() {
        let mut data = vec![0xaa; 1000];
        let body = b"LYRICSBEGININD0000000lyrics text here";
        data.extend_from_slice(body);
        data.extend_from_slice(format!("{:06}LYRICS200", body.len()).as_bytes());
        data.extend(id3v1());
        assert_eq!(end_of(data, 0), 1000);
    }

    #[test]
    fn verify_lyrics3v1_is_skipped() {
        let mut data = vec![0xaa; 1000];
        data.extend_from_slice(b"LYRICSBEGINsome lyrics text LYRICSEND");
        assert_eq!(end_of(data, 0), 1000);
    }

    #[test]
    fn verify_id3v2_footer_is_skipped() {
        let mut data = vec![0xaa; 1000];
        // Header, 20 bytes of frames/padding, footer.
        data.extend_from_slice(b"ID3\x04\x00\x10\x00\x00\x00\x14");
        data.extend_from_slice(&[0; 20]);
        data.extend_from_slice(b"3DI\x04\x00\x10\x00\x00\x00\x14");
        assert_eq!(end_of(data, 0), 1000);
    }

    #[test]
    fn verify_bogus_ape_footer_is_ignored() {
        let mut data = vec![0xaa; 1000];
        let mut footer = Vec::new();
        footer.extend_from_slice(b"APETAGEX");
        footer.extend_from_slice(&2000u32.to_le_bytes());
        // A size larger than the stream.
        footer.extend_from_slice(&0xffff_0000u32.to_le_bytes());
        footer.extend_from_slice(&[0; 16]);
        data.extend(footer);
        let len = data.len() as u64;
        assert_eq!(end_of(data, 0), len);
    }
}
