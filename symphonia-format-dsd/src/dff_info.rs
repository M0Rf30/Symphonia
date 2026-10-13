// DFF INFO Chunk Parser
// Handles metadata from DSDIFF INFO chunks

use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::*;
use symphonia_core::meta::{MetadataBuilder, MetadataId, MetadataInfo, RawTag, Tag};

/// DFF INFO chunk parser
pub struct DffInfoParser {
    metadata: MetadataBuilder,
}

impl DffInfoParser {
    pub fn new() -> Self {
        DffInfoParser {
            metadata: MetadataBuilder::new(MetadataInfo {
                metadata: MetadataId::new(FourCc::new(*b"DFFI")),
                short_name: "dff_info",
                long_name: "DSDIFF INFO",
            }),
        }
    }

    /// Parse a DIIN container chunk (holds DITI/DIAR sub-chunks). `size` is the DIIN payload size;
    /// every read is bounded by it. The caller is responsible for positioning the stream at the end
    /// of the chunk afterwards.
    pub fn parse_diin(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        let mut remaining = size;
        while remaining >= DSDIFF_CHUNK_HEADER_LEN {
            let (id, sub) = read_chunk_header(reader)?;
            remaining -= DSDIFF_CHUNK_HEADER_LEN;
            let padded = sub.saturating_add(sub & 1);
            if padded > remaining {
                return decode_error("dff: DIIN sub-chunk exceeds chunk");
            }
            match &id {
                b"DITI" | b"DIAR" => {
                    let key = if &id == b"DITI" { "TITLE" } else { "ARTIST" };
                    let count = reader.read_be_u32()?;
                    if u64::from(count) + 4 > sub {
                        return decode_error("dff: DIIN text exceeds sub-chunk");
                    }
                    let data = reader.read_boxed_slice_exact(count as usize)?;
                    let s = String::from_utf8_lossy(&data).trim().to_string();
                    if !s.is_empty() {
                        self.metadata.add_tag(Tag::new(RawTag::new(key, s)));
                    }
                    reader.ignore_bytes(padded - 4 - u64::from(count))?;
                }
                _ => reader.ignore_bytes(padded)?,
            }
            remaining -= padded;
        }
        Ok(())
    }

    /// Parse a COMT top-level chunk. `size` is the COMT payload size; every read is bounded by it.
    /// The caller is responsible for positioning the stream at the end of the chunk afterwards.
    pub fn parse_comt(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        if size < 2 {
            return decode_error("dff: COMT chunk too small");
        }
        let num = reader.read_be_u16()?;
        let mut consumed: u64 = 2;
        for _ in 0..num {
            if consumed + 14 > size {
                break;
            }
            let _year = reader.read_be_u16()?;
            let _month = reader.read_u8()?;
            let _day = reader.read_u8()?;
            let _hour = reader.read_u8()?;
            let _minutes = reader.read_u8()?;
            let _cmt_type = reader.read_be_u16()?;
            let _cmt_ref = reader.read_be_u16()?;
            let count = reader.read_be_u32()?;
            let pad = u64::from(count & 1);
            consumed += 14;
            if u64::from(count) + pad > size - consumed {
                return decode_error("dff: COMT text exceeds chunk");
            }
            let data = reader.read_boxed_slice_exact(count as usize)?;
            let s = String::from_utf8_lossy(&data).trim().to_string();
            if !s.is_empty() {
                self.metadata.add_tag(Tag::new(RawTag::new("COMMENT", s)));
            }
            if pad == 1 {
                reader.ignore_bytes(1)?;
            }
            consumed += u64::from(count) + pad;
        }
        Ok(())
    }

    pub fn into_metadata(self) -> MetadataBuilder {
        self.metadata
    }
}

/// Length in bytes of a DSDIFF chunk header (4-byte id and 8-byte size).
pub(crate) const DSDIFF_CHUNK_HEADER_LEN: u64 = 12;

/// Read a DSDIFF chunk header: a 4-byte id followed by a big-endian u64 payload size.
pub(crate) fn read_chunk_header(reader: &mut MediaSourceStream<'_>) -> Result<([u8; 4], u64)> {
    let id = reader.read_quad_bytes()?;
    let size = reader.read_be_u64()?;
    Ok((id, size))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
        MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
    }

    #[test]
    fn verify_diin_text_count_exceeding_subchunk_is_rejected() {
        // DITI sub-chunk declares 8 bytes but the text count claims 100.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"DITI");
        buf.extend_from_slice(&8u64.to_be_bytes());
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(&[b'a'; 200]);
        let mut parser = DffInfoParser::new();
        let mut s = mss(buf);
        assert!(parser.parse_diin(&mut s, 20).is_err());
        // Nothing past the 4-byte count may have been consumed.
        assert_eq!(s.pos(), 12 + 4);
    }

    #[test]
    fn verify_diin_valid_title() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"DITI");
        buf.extend_from_slice(&7u64.to_be_bytes());
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(b"abc");
        buf.push(0); // pad
        let mut parser = DffInfoParser::new();
        let mut s = mss(buf);
        parser.parse_diin(&mut s, 20).unwrap();
        assert_eq!(s.pos(), 20);
    }

    #[test]
    fn verify_comt_text_count_exceeding_chunk_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&[b'a'; 2000]);
        let mut parser = DffInfoParser::new();
        let mut s = mss(buf);
        assert!(parser.parse_comt(&mut s, 16).is_err());
        assert_eq!(s.pos(), 16);
    }
}
