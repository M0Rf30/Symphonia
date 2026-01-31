// DFF INFO Chunk Parser
// Handles metadata from DSDIFF INFO chunks

use symphonia_core::errors::Result;
use symphonia_core::io::*;
use symphonia_core::meta::{MetadataBuilder, StandardTagKey, Tag, Value};

use log::{debug, warn};

/// DFF INFO chunk parser
pub struct DffInfoParser {
    metadata: MetadataBuilder,
}

impl DffInfoParser {
    /// Create a new INFO parser
    pub fn new() -> Self {
        DffInfoParser { metadata: MetadataBuilder::new() }
    }

    /// Parse an INFO chunk and add tags to metadata
    pub fn parse_chunk(&mut self, chunk_id: &[u8; 4], reader: &mut MediaSourceStream, size: u64) -> Result<()> {
        match chunk_id {
            b"DIIN" => self.parse_diin(reader, size),
            b"DITI" => self.parse_text_chunk(reader, size, StandardTagKey::TrackTitle),
            b"DIAR" => self.parse_text_chunk(reader, size, StandardTagKey::Artist),
            b"DISP" => self.parse_text_chunk(reader, size, StandardTagKey::Album),
            b"DIGE" => self.parse_text_chunk(reader, size, StandardTagKey::Genre),
            b"COMT" => self.parse_comt(reader, size),
            _ => {
                // Unknown INFO chunk, skip it
                debug!("DFF INFO: Skipping unknown chunk: {}", String::from_utf8_lossy(chunk_id));
                reader.ignore_bytes(size)?;
                Ok(())
            }
        }
    }

    /// Parse DIIN (composite INFO) chunk containing multiple sub-chunks
    fn parse_diin(&mut self, reader: &mut MediaSourceStream, size: u64) -> Result<()> {
        let end_pos = reader.pos() + size;

        while reader.pos() < end_pos {
            let chunk_id = reader.read_quad_bytes()?;
            let chunk_size = reader.read_be_u64()?;

            self.parse_chunk(&chunk_id, reader, chunk_size)?;

            // Handle padding (DSDIFF uses 2-byte alignment)
            if chunk_size % 2 == 1 {
                reader.ignore_bytes(1)?;
            }
        }

        Ok(())
    }

    /// Parse a simple text chunk
    fn parse_text_chunk(&mut self, reader: &mut MediaSourceStream, size: u64, key: StandardTagKey) -> Result<()> {
        if size == 0 {
            return Ok(());
        }

        // Read the text data
        let data = reader.read_boxed_slice_exact(size as usize)?;

        // Decode as UTF-8 or ASCII
        if let Some(text) = decode_dff_text(&data) {
            debug!("DFF INFO: {:?} = {}", key, text);
            self.metadata.add_tag(Tag::new(Some(key), "DFF-INFO", Value::from(text)));
        }
        else {
            warn!("DFF INFO: Failed to decode text for {:?}", key);
        }

        Ok(())
    }

    /// Parse COMT (comment) chunk which has a more complex structure
    fn parse_comt(&mut self, reader: &mut MediaSourceStream, size: u64) -> Result<()> {
        let end_pos = reader.pos() + size;

        // COMT contains a count followed by comment records
        let num_comments = reader.read_be_u16()?;

        for _ in 0..num_comments {
            if reader.pos() >= end_pos {
                break;
            }

            // Each comment has: year (2), month (1), day (1), hour (1), minute (1)
            let _year = reader.read_be_u16()?;
            let _month = reader.read_u8()?;
            let _day = reader.read_u8()?;
            let _hour = reader.read_u8()?;
            let _minute = reader.read_u8()?;

            // Comment type (2 bytes)
            let _comment_type = reader.read_be_u16()?;

            // Comment string count (2 bytes) - number of text strings
            let string_count = reader.read_be_u16()?;

            // Read comment strings
            for _ in 0..string_count {
                // Each string has a 4-byte size followed by text
                let text_size = reader.read_be_u32()?;

                if text_size > 0 {
                    let text_data = reader.read_boxed_slice_exact(text_size as usize)?;

                    if let Some(text) = decode_dff_text(&text_data) {
                        debug!("DFF INFO: Comment = {}", text);
                        self.metadata.add_tag(Tag::new(
                            Some(StandardTagKey::Comment),
                            "DFF-INFO",
                            Value::from(text),
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Consume the parser and return the built metadata
    pub fn into_metadata(self) -> MetadataBuilder {
        self.metadata
    }
}

/// Decode DFF text data (UTF-8 or ASCII)
fn decode_dff_text(data: &[u8]) -> Option<String> {
    // Remove null terminators and trailing nulls
    let trimmed = data.iter()
        .position(|&b| b == 0)
        .map(|pos| &data[..pos])
        .unwrap_or(data);

    if trimmed.is_empty() {
        return None;
    }

    // Try UTF-8 first
    if let Ok(text) = std::str::from_utf8(trimmed) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    // Fall back to lossy conversion for non-UTF8
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
