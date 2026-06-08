// DFF INFO Chunk Parser
// Handles metadata from DSDIFF INFO chunks

use symphonia_core::errors::Result;
use symphonia_core::io::*;
use symphonia_core::common::FourCc;
use symphonia_core::meta::{MetadataBuilder, MetadataId, MetadataInfo, RawTag, Tag};

/// DFF INFO chunk parser
pub struct DffInfoParser {
    metadata: MetadataBuilder,
}

impl DffInfoParser {
    pub fn new() -> Self {
        DffInfoParser { metadata: MetadataBuilder::new(MetadataInfo { metadata: MetadataId::new(FourCc::new(*b"DFFI")), short_name: "dff_info", long_name: "DSDIFF INFO" }) }
    }

    /// Parse a DIIN container chunk (holds DITI/DIAR sub-chunks).
    pub fn parse_diin(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        let mut consumed: u64 = 0;
        while consumed < size {
            let id = reader.read_quad_bytes()?;
            let sub = reader.read_be_u64()?;
            match &id {
                b"DITI" | b"DIAR" => {
                    let key = if &id == b"DITI" { "TITLE" } else { "ARTIST" };
                    let count = reader.read_be_u32()?;
                    let data = reader.read_boxed_slice_exact(count as usize)?;
                    let s = String::from_utf8_lossy(&data).trim().to_string();
                    if !s.is_empty() { self.metadata.add_tag(Tag::new(RawTag::new(key, s))); }
                    let header_and_count: u64 = 4 + count as u64;
                    if sub > header_and_count { reader.ignore_bytes(sub - header_and_count)?; }
                }
                _ => { reader.ignore_bytes(sub)?; }
            }
            let pad = sub & 1;
            if pad == 1 { reader.ignore_bytes(1)?; }
            consumed += 12 + sub + pad;
        }
        Ok(())
    }

    /// Parse a COMT top-level chunk.
    pub fn parse_comt(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        let num = reader.read_be_u16()?;
        let mut consumed: u64 = 2;
        for _ in 0..num {
            if consumed >= size { break; }
            let _year = reader.read_be_u16()?;
            let _month = reader.read_u8()?;
            let _day = reader.read_u8()?;
            let _hour = reader.read_u8()?;
            let _minutes = reader.read_u8()?;
            let _cmt_type = reader.read_be_u16()?;
            let _cmt_ref = reader.read_be_u16()?;
            let count = reader.read_be_u32()?;
            let data = reader.read_boxed_slice_exact(count as usize)?;
            let s = String::from_utf8_lossy(&data).trim().to_string();
            if !s.is_empty() { self.metadata.add_tag(Tag::new(RawTag::new("COMMENT", s))); }
            let pad = count & 1;
            if pad == 1 { reader.ignore_bytes(1)?; }
            consumed += 14 + count as u64 + pad as u64;
        }
        Ok(())
    }

    pub fn into_metadata(self) -> MetadataBuilder {
        self.metadata
    }
}
