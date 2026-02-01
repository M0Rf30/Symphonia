# Symphonia DSD Format Demuxer

Pure Rust demuxer for DSD (Direct Stream Digital) audio formats.

## Supported Formats

- **DSF** (DSD Stream File) - Sony's DSD format ✅ Fully Implemented
- **DFF** (DSDIFF - DSD Interchange File Format) - Philips/Sony IFF-based format ✅ Fully Implemented

## Features

### Format Support
- **DSD sample rates**: DSD64 (2.8224 MHz), DSD128 (5.6448 MHz), DSD256 (11.2896 MHz), DSD512 (22.5792 MHz)
- **Multi-channel audio**: Mono, stereo, 2.1, 5.1, and custom channel configurations (up to 6 channels)
- **Native DSD output**: U8 format (1 byte = 8 DSD bits) for native DSD playback
- **Seeking support**: Sample-accurate seeking in both DSF and DFF formats

### Metadata Extraction
- **DSF**: ID3v2 tag parsing (artist, title, album, genre, comments, cover art, etc.)
- **DFF**: INFO chunk parsing (DITI/title, DIAR/artist, DISP/album, DIGE/genre, COMT/comments)

### Validation & Error Handling
- Comprehensive format validation (magic numbers, chunk sizes, channel counts)
- Channel ID validation for DFF files (SLFT, SRGT, MLFT, MRGT, C, LFE, LS, RS)
- Descriptive error messages with actionable suggestions
- Graceful handling of malformed files

### Known Limitations
- **DST compression not supported**: DFF files using DST (Direct Stream Transfer) compression will be rejected with a helpful error message suggesting conversion tools
- **Maximum 6 channels**: Channel counts beyond 6 are not currently supported

## Usage

This crate is part of the Symphonia project and is designed to be used with the `symphonia` meta-crate or standalone with `symphonia-core`.

### Example

```rust
use symphonia_core::formats::FormatReader;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_dsd::DsfReader;
use std::fs::File;

// Open a DSF file
let file = File::open("audio.dsf")?;
let mss = MediaSourceStream::new(Box::new(file), Default::default());

// Create format reader
let mut reader = DsfReader::try_new(mss, &Default::default())?;

// Access metadata
let metadata = reader.metadata();

// Read packets
while let Ok(packet) = reader.next_packet() {
    // Process DSD audio data
}
```

## Format Specifications

This implementation is based on:
- DSF v1.01 specification
- DSDIFF v1.5 specification (Philips/Sony)

## Testing

Comprehensive test coverage includes:
- Unit tests for individual components (text decoding, validation)
- Integration tests for format parsing (valid/invalid files, edge cases)
- Error handling tests (truncated files, invalid headers, unsupported features)

Run tests with:
```bash
cargo test --package symphonia-format-dsd
```

## License

This project is licensed under the Mozilla Public License 2.0.
