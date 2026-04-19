// DFF INFO Chunk Parser
// Handles metadata from DSDIFF INFO chunks

use std::sync::Arc;

use symphonia_core::common::FourCc;
use symphonia_core::errors::Result;
use symphonia_core::io::*;
use symphonia_core::meta::{
    MetadataBuilder, MetadataId, MetadataInfo, MetadataRevision, RawTag, StandardTag, Tag,
};

use log::{debug, warn};

static DFF_INFO_METADATA_INFO: MetadataInfo = MetadataInfo {
    metadata: MetadataId::new(FourCc::new(*b"DFFI")),
    short_name: "dff-info",
    long_name: "DFF INFO Chunk",
};

pub struct DffInfoParser {
    metadata: MetadataBuilder,
}

impl DffInfoParser {
    pub fn new() -> Self {
        DffInfoParser { metadata: MetadataBuilder::new(DFF_INFO_METADATA_INFO) }
    }

    pub fn parse_chunk(&mut self, chunk_id: &[u8; 4], reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        match chunk_id {
            b"DIIN" => self.parse_diin(reader, size),
            b"DITI" => self.parse_text_chunk_with(reader, size, |text| StandardTag::TrackTitle(Arc::new(text))),
            b"DIAR" => self.parse_text_chunk_with(reader, size, |text| StandardTag::Artist(Arc::new(text))),
            b"DISP" => self.parse_text_chunk_with(reader, size, |text| StandardTag::Album(Arc::new(text))),
            b"DIGE" => self.parse_text_chunk_with(reader, size, |text| StandardTag::Genre(Arc::new(text))),
            b"COMT" => self.parse_comt(reader, size),
            _ => {
                debug!("DFF INFO: Skipping unknown chunk: {}", String::from_utf8_lossy(chunk_id));
                reader.ignore_bytes(size)?;
                Ok(())
            }
        }
    }

    fn parse_diin(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        let end_pos = reader.pos() + size;

        while reader.pos() < end_pos {
            let chunk_id = reader.read_quad_bytes()?;
            let chunk_size = reader.read_be_u64()?;

            self.parse_chunk(&chunk_id, reader, chunk_size)?;

            if chunk_size % 2 == 1 {
                reader.ignore_bytes(1)?;
            }
        }

        Ok(())
    }

    fn parse_text_chunk_with<F>(
        &mut self,
        reader: &mut MediaSourceStream<'_>,
        size: u64,
        make_tag: F,
    ) -> Result<()>
    where
        F: FnOnce(String) -> StandardTag,
    {
        if size == 0 {
            return Ok(());
        }

        let data = reader.read_boxed_slice_exact(size as usize)?;

        if let Some(text) = decode_dff_text(&data) {
            debug!("DFF INFO: {} = {}", std::any::type_name::<F>(), text);
            let std_tag = make_tag(text.clone());
            let raw = RawTag::new("DFF-INFO", text);
            self.metadata.add_tag(Tag::new_std(raw, std_tag));
        }
        else {
            warn!("DFF INFO: Failed to decode text");
        }

        Ok(())
    }

    fn parse_comt(&mut self, reader: &mut MediaSourceStream<'_>, size: u64) -> Result<()> {
        let end_pos = reader.pos() + size;

        let num_comments = reader.read_be_u16()?;

        for _ in 0..num_comments {
            if reader.pos() >= end_pos {
                break;
            }

            let _year = reader.read_be_u16()?;
            let _month = reader.read_u8()?;
            let _day = reader.read_u8()?;
            let _hour = reader.read_u8()?;
            let _minute = reader.read_u8()?;

            let _comment_type = reader.read_be_u16()?;

            let string_count = reader.read_be_u16()?;

            for _ in 0..string_count {
                let text_size = reader.read_be_u32()?;

                if text_size > 0 {
                    let text_data = reader.read_boxed_slice_exact(text_size as usize)?;

                    if let Some(text) = decode_dff_text(&text_data) {
                        debug!("DFF INFO: Comment = {}", text);
                        let raw = RawTag::new("DFF-INFO", text.clone());
                        self.metadata.add_tag(Tag::new_std(
                            raw,
                            StandardTag::Comment(text.into()),
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    pub fn into_revision(self) -> MetadataRevision {
        self.metadata.build()
    }
}

fn decode_dff_text(data: &[u8]) -> Option<String> {
    let trimmed = data.iter()
        .position(|&b| b == 0)
        .map(|pos| &data[..pos])
        .unwrap_or(data);

    if trimmed.is_empty() {
        return None;
    }

    if let Ok(text) = std::str::from_utf8(trimmed) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let text = String::from_utf8_lossy(trimmed);
    let text = text.trim();

    if !text.is_empty() {
        Some(text.to_string())
    }
    else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_dff_text_utf8() {
        let data = b"Test String";
        assert_eq!(decode_dff_text(data), Some("Test String".to_string()));
    }

    #[test]
    fn test_decode_dff_text_null_terminated() {
        let data = b"Test String\0\0\0";
        assert_eq!(decode_dff_text(data), Some("Test String".to_string()));
    }

    #[test]
    fn test_decode_dff_text_empty() {
        let data = b"";
        assert_eq!(decode_dff_text(data), None);
    }

    #[test]
    fn test_decode_dff_text_only_nulls() {
        let data = b"\0\0\0";
        assert_eq!(decode_dff_text(data), None);
    }
}
