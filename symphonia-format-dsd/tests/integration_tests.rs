// Integration tests for DSD format parsers

use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_dsd::{DsfReader, DffReader};
use std::io::Cursor;
use symphonia_core::formats::{SeekMode, SeekTo};
use symphonia_core::units::Timestamp;
use symphonia_core::codecs::audio::{ChannelDataLayout, BitOrder};
use symphonia_core::audio::{Channels, Position};

// Helper to create a MediaSourceStream from bytes
fn create_stream(data: Vec<u8>) -> MediaSourceStream<'static> {
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
    let result = DsfReader::try_new(stream, Default::default());
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
    let result = DsfReader::try_new(stream, Default::default());
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
    let result = DsfReader::try_new(stream, Default::default());
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
    let result = DsfReader::try_new(stream, Default::default());
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
    let result = DsfReader::try_new(stream, Default::default());
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
    let result = DffReader::try_new(stream, Default::default());
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
    let result = DffReader::try_new(stream, Default::default());
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
    let result = DffReader::try_new(stream, Default::default());
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
    let result = DffReader::try_new(stream, Default::default());
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
    let result = DffReader::try_new(stream, Default::default());
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
// ---------------------------------------------------------------------------
// Byte-builder helpers
// ---------------------------------------------------------------------------

fn build_dsf(channel_type: u32, channels: u32, rate: u32, block: u32, sample_count: u64, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"DSD ");
    v.extend_from_slice(&28u64.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes()); // file size (unused by reader)
    v.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer = 0 (none)
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&52u64.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&channel_type.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&sample_count.to_le_bytes());
    v.extend_from_slice(&block.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&((data.len() as u64) + 12).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn be_u64(n: u64) -> [u8; 8] { n.to_be_bytes() }

fn diin(title: &str, artist: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    for (id, txt) in [(b"DITI" as &[u8], title), (b"DIAR", artist)] {
        inner.extend_from_slice(id);
        inner.extend_from_slice(&be_u64(4 + txt.len() as u64));
        inner.extend_from_slice(&(txt.len() as u32).to_be_bytes());
        inner.extend_from_slice(txt.as_bytes());
        if txt.len() % 2 == 1 { inner.push(0); }
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"DIIN");
    v.extend_from_slice(&be_u64(inner.len() as u64));
    v.extend_from_slice(&inner);
    v
}

fn comt(text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_be_bytes()); // numComments
    body.extend_from_slice(&[0, 0]); // year
    body.extend_from_slice(&[0, 0, 0, 0]); // month,day,hour,minutes
    body.extend_from_slice(&[0, 0]); // cmtType
    body.extend_from_slice(&[0, 0]); // cmtRef
    body.extend_from_slice(&(text.len() as u32).to_be_bytes());
    body.extend_from_slice(text.as_bytes());
    if text.len() % 2 == 1 { body.push(0); }
    let mut v = Vec::new();
    v.extend_from_slice(b"COMT");
    v.extend_from_slice(&be_u64(body.len() as u64));
    v.extend_from_slice(&body);
    v
}

fn build_dff(channels: u16, rate: u32, ch_ids: &[&[u8; 4]], data: &[u8], trailer: &[u8]) -> Vec<u8> {
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  "); prop.extend_from_slice(&be_u64(4)); prop.extend_from_slice(&rate.to_be_bytes());
    let chnl = 2u64 + 4 * ch_ids.len() as u64;
    prop.extend_from_slice(b"CHNL"); prop.extend_from_slice(&be_u64(chnl));
    prop.extend_from_slice(&channels.to_be_bytes());
    for id in ch_ids { prop.extend_from_slice(*id); }
    if chnl % 2 == 1 { prop.push(0); }
    prop.extend_from_slice(b"CMPR"); prop.extend_from_slice(&be_u64(4)); prop.extend_from_slice(b"DSD ");
    let mut v = Vec::new();
    v.extend_from_slice(b"FRM8");
    v.extend_from_slice(&be_u64(0)); // patched below
    v.extend_from_slice(b"DSD ");
    v.extend_from_slice(b"FVER"); v.extend_from_slice(&be_u64(4)); v.extend_from_slice(&0x0100_0000u32.to_be_bytes());
    v.extend_from_slice(b"PROP"); v.extend_from_slice(&be_u64(prop.len() as u64)); v.extend_from_slice(&prop);
    if prop.len() % 2 == 1 { v.push(0); }
    v.extend_from_slice(b"DSD "); v.extend_from_slice(&be_u64(data.len() as u64)); v.extend_from_slice(data);
    if data.len() % 2 == 1 { v.push(0); }
    v.extend_from_slice(trailer);
    let total = v.len() as u64 - 12;
    v[4..12].copy_from_slice(&total.to_be_bytes());
    v
}

// ---------------------------------------------------------------------------
// Spec-compliance regression tests
// ---------------------------------------------------------------------------

/// DSF: per-channel frame durations, Planar layout, Positioned stereo channels.
#[test]
fn test_dsf_stereo_per_channel_duration() {
    // block=16 per channel, 2 channels → 32-byte block group; 128 bytes = 4 groups.
    // Each packet: 32 bytes, dur = (32*8)/2 = 128 frames/ch.
    let raw = build_dsf(2, 2, 2822400, 16, 512, &[0u8; 128]);
    let stream = create_stream(raw);
    let mut reader = DsfReader::try_new(stream, Default::default()).unwrap();

    let num_frames = reader.tracks()[0].num_frames.unwrap();
    assert_eq!(num_frames, 512);

    let audio = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone();
    assert_eq!(audio.channel_data_layout, Some(ChannelDataLayout::Planar));
    assert_eq!(
        audio.channels,
        Some(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT))
    );

    let mut packet_count = 0u32;
    let mut total_dur = 0u64;
    while let Ok(Some(p)) = reader.next_packet() {
        assert_eq!(p.data.len(), 32, "packet {} data len", packet_count);
        assert_eq!(p.dur.get(), 128, "packet {} dur", packet_count);
        total_dur += p.dur.get();
        packet_count += 1;
    }
    assert_eq!(packet_count, 4);
    assert_eq!(total_dur, 512);
    assert_eq!(total_dur, num_frames);
}

/// DSF: trailing padding is trimmed so sum(durations) == sample_count, not rounded-up capacity.
#[test]
fn test_dsf_trailing_padding_trim() {
    // sample_count=500; 4 block groups still needed, last packet dur trimmed to 116.
    let raw = build_dsf(2, 2, 2822400, 16, 500, &[0u8; 128]);
    let stream = create_stream(raw);
    let mut reader = DsfReader::try_new(stream, Default::default()).unwrap();

    let num_frames = reader.tracks()[0].num_frames.unwrap();
    assert_eq!(num_frames, 500);

    let mut total_dur = 0u64;
    let mut last_dur = 0u64;
    while let Ok(Some(p)) = reader.next_packet() {
        last_dur = p.dur.get();
        total_dur += last_dur;
    }
    assert_eq!(total_dur, 500);
    assert_eq!(last_dur, 116); // 500 - 3*128 = 116
}

/// DSF: seek snaps to block boundary; block holds block_size*8 = 16*8 = 128 frames/ch.
#[test]
fn test_dsf_block_aligned_seek() {
    let raw = build_dsf(2, 2, 2822400, 16, 512, &[0u8; 128]);
    let stream = create_stream(raw);
    let mut reader = DsfReader::try_new(stream, Default::default()).unwrap();

    // Seek to frame 200; block_idx = 200/128 = 1 → actual = 1*128 = 128.
    let result = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(200), track_id: 0 })
        .unwrap();
    assert_eq!(result.actual_ts.get(), 128);
}

/// DFF: per-channel frame count equals total_bytes*8/channels; Interleaved layout; MsbFirst.
#[test]
fn test_dff_stereo_per_channel_duration() {
    // 64 bytes, 2 channels → samples_per_channel = 64*8/2 = 256.
    let raw = build_dff(2, 2822400, &[b"SLFT", b"SRGT"], &[0u8; 64], &[]);
    let stream = create_stream(raw);
    let mut reader = DffReader::try_new(stream, Default::default()).unwrap();

    let num_frames = reader.tracks()[0].num_frames.unwrap();
    assert_eq!(num_frames, 256);

    let audio = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap().clone();
    assert_eq!(audio.channel_data_layout, Some(ChannelDataLayout::Interleaved));
    assert_eq!(audio.bit_order, Some(BitOrder::MsbFirst));
    assert_eq!(
        audio.channels,
        Some(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT))
    );

    let mut total_dur = 0u64;
    while let Ok(Some(p)) = reader.next_packet() {
        total_dur += p.dur.get();
    }
    assert_eq!(total_dur, 256);
    assert_eq!(total_dur, num_frames);
}

/// DFF: every packet data length is an exact multiple of channel_count (byte-interleaved).
#[test]
fn test_dff_packet_channel_alignment() {
    // 5.1 surround, 8184 bytes; 8184 % 6 == 0 → 10912 frames/ch.
    // Aligned chunk = (4096/6)*6 = 4092 bytes per packet.
    let raw = build_dff(
        6,
        2822400,
        &[b"MLFT", b"MRGT", b"C   ", b"LFE ", b"LS  ", b"RS  "],
        &[0u8; 8184],
        &[],
    );
    let stream = create_stream(raw);
    let mut reader = DffReader::try_new(stream, Default::default()).unwrap();

    let num_frames = reader.tracks()[0].num_frames.unwrap();
    assert_eq!(num_frames, 8184 * 8 / 6); // 10912

    let mut total_dur = 0u64;
    while let Ok(Some(p)) = reader.next_packet() {
        assert_eq!(p.data.len() % 6, 0, "packet not aligned to channel count");
        total_dur += p.dur.get();
    }
    assert_eq!(total_dur, num_frames);
}

/// DFF: DIIN (TITLE/ARTIST) and COMT (COMMENT) trailer chunks are parsed into metadata.
#[test]
fn test_dff_metadata_diin_comt() {
    let mut trailer = diin("My Title", "My Artist");
    trailer.extend(comt("Hello"));
    let raw = build_dff(2, 2822400, &[b"SLFT", b"SRGT"], &[0u8; 64], &trailer);
    let stream = create_stream(raw);
    let mut reader = DffReader::try_new(stream, Default::default()).unwrap();

    let rev = reader.metadata().current().cloned()
        .expect("expected a metadata revision");
    let tags = &rev.media.tags;

    let title_tag = tags.iter().find(|t| t.raw.key == "TITLE")
        .expect("TITLE tag missing");
    assert!(
        format!("{}", title_tag.raw.value).contains("My Title"),
        "TITLE value did not contain 'My Title': {:?}", title_tag.raw.value
    );

    let artist_tag = tags.iter().find(|t| t.raw.key == "ARTIST")
        .expect("ARTIST tag missing");
    assert!(
        format!("{}", artist_tag.raw.value).contains("My Artist"),
        "ARTIST value did not contain 'My Artist': {:?}", artist_tag.raw.value
    );

    let comment_tag = tags.iter().find(|t| t.raw.key == "COMMENT")
        .expect("COMMENT tag missing");
    assert!(
        format!("{}", comment_tag.raw.value).contains("Hello"),
        "COMMENT value did not contain 'Hello': {:?}", comment_tag.raw.value
    );
}
