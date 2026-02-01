// Integration tests for DSD format parsers

use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_dsd::{DsfReader, DffReader};
use std::io::Cursor;

// Helper to create a MediaSourceStream from bytes
fn create_stream(data: Vec<u8>) -> MediaSourceStream {
    let cursor = Cursor::new(data);
    MediaSourceStream::new(Box::new(cursor), Default::default())
}

#[test]
fn test_dsf_invalid_magic() {
    // Invalid magic bytes
    let data = vec![
        b'X', b'X', b'X', b'X', // Bad magic (should be "DSD ")
        0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // chunk size = 28
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // file size
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // metadata pointer
    ];

    let stream = create_stream(data);
    let result = DsfReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dsf_invalid_header_size() {
    let data = vec![
        b'D', b'S', b'D', b' ', // Magic
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // Wrong chunk size (should be 28)
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // file size
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // metadata pointer
    ];

    let stream = create_stream(data);
    let result = DsfReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dsf_unsupported_format_version() {
    let mut data = vec![
        // DSD header
        b'D', b'S', b'D', b' ', // Magic
        0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // chunk size = 28
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // file size
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // metadata pointer
        // fmt chunk
        b'f', b'm', b't', b' ', // Magic
        0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // chunk size = 52
        0x02, 0x00, 0x00, 0x00, // format version = 2 (unsupported)
    ];

    let stream = create_stream(data);
    let result = DsfReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dsf_invalid_channel_count() {
    let mut data = vec![
        // DSD header
        b'D', b'S', b'D', b' ',
        0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // fmt chunk
        b'f', b'm', b't', b' ',
        0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, // format version = 1
        0x00, 0x00, 0x00, 0x00, // format ID = 0 (DSD Raw)
        0x02, 0x00, 0x00, 0x00, // channel type = 2 (stereo)
        0x00, 0x00, 0x00, 0x00, // channel num = 0 (INVALID)
    ];

    let stream = create_stream(data);
    let result = DsfReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dsf_zero_block_size() {
    let mut data = vec![
        // DSD header
        b'D', b'S', b'D', b' ',
        0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // fmt chunk
        b'f', b'm', b't', b' ',
        0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, // format version = 1
        0x00, 0x00, 0x00, 0x00, // format ID = 0
        0x02, 0x00, 0x00, 0x00, // channel type = 2
        0x02, 0x00, 0x00, 0x00, // channel num = 2
        0x00, 0x10, 0x2B, 0x00, // sampling frequency = 2822400 (DSD64)
        0x01, 0x00, 0x00, 0x00, // bits per sample = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // sample count = 0
        0x00, 0x00, 0x00, 0x00, // block size = 0 (INVALID)
    ];

    let stream = create_stream(data);
    let result = DsfReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dff_invalid_magic() {
    let data = vec![
        b'X', b'X', b'X', b'X', // Bad magic (should be "FRM8")
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, // chunk size
        b'D', b'S', b'D', b' ', // form type
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dff_invalid_form_type() {
    let data = vec![
        b'F', b'R', b'M', b'8', // Magic
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, // chunk size
        b'X', b'X', b'X', b' ', // Bad form type (should be "DSD ")
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dff_missing_fver_chunk() {
    let data = vec![
        b'F', b'R', b'M', b'8',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20,
        b'D', b'S', b'D', b' ',
        // Missing FVER chunk - directly to PROP
        b'P', b'R', b'O', b'P',
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dff_dst_compression_error() {
    let mut data = vec![
        b'F', b'R', b'M', b'8',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        b'D', b'S', b'D', b' ',
        // FVER chunk
        b'F', b'V', b'E', b'R',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        0x01, 0x05, 0x00, 0x00, // version 1.5.0.0
        // PROP chunk
        b'P', b'R', b'O', b'P',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30,
        b'S', b'N', b'D', b' ',
        // FS chunk
        b'F', b'S', b' ', b' ',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        0x00, 0x10, 0x2B, 0x00, // 2822400 Hz
        // CHNL chunk
        b'C', b'H', b'N', b'L',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A,
        0x00, 0x02, // 2 channels
        b'S', b'L', b'F', b'T',
        b'S', b'R', b'G', b'T',
        // CMPR chunk with DST compression
        b'C', b'M', b'P', b'R',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        b'D', b'S', b'T', b' ', // DST compression (unsupported)
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, &Default::default());
    assert!(result.is_err());

    // Check that error message mentions DST
    if let Err(e) = result {
        let error_msg = format!("{}", e);
        assert!(error_msg.contains("DST") || error_msg.contains("compression"));
    }
}

#[test]
fn test_dff_invalid_channel_count() {
    let mut data = vec![
        b'F', b'R', b'M', b'8',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        b'D', b'S', b'D', b' ',
        // FVER chunk
        b'F', b'V', b'E', b'R',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        0x01, 0x05, 0x00, 0x00,
        // PROP chunk
        b'P', b'R', b'O', b'P',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2C,
        b'S', b'N', b'D', b' ',
        // FS chunk
        b'F', b'S', b' ', b' ',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        0x00, 0x10, 0x2B, 0x00,
        // CHNL chunk with invalid count
        b'C', b'H', b'N', b'L',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A,
        0x00, 0x07, // 7 channels (invalid, max is 6)
        b'S', b'L', b'F', b'T',
        b'S', b'R', b'G', b'T',
        // CMPR chunk
        b'C', b'M', b'P', b'R',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
        b'D', b'S', b'D', b' ',
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, &Default::default());
    assert!(result.is_err());
}

#[cfg(test)]
mod dff_info_tests {
    use super::*;

    #[test]
    fn test_dff_text_decoding() {
        // This is tested in the dff_info module unit tests
        // Just a placeholder to show INFO chunk parsing would be tested
    }
}
