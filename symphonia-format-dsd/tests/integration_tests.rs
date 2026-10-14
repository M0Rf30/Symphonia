// Integration tests for DSD format parsers
#![allow(clippy::unwrap_used)]

use std::io::Cursor;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::{BitOrder, ChannelDataLayout};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::{SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_format_dsd::{DffReader, DsfReader};

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
    let data = vec![
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
    let data = vec![
        // DSD header
        b'D', b'S', b'D', b' ', 0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // fmt chunk
        b'f', b'm', b't', b' ', 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, // format version = 1
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
    let data = vec![
        // DSD header
        b'D', b'S', b'D', b' ', 0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // fmt chunk
        b'f', b'm', b't', b' ', 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, // format version = 1
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
        b'F', b'R', b'M', b'8', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, b'D', b'S', b'D',
        b' ', // Missing FVER chunk - directly to PROP
        b'P', b'R', b'O', b'P',
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, Default::default());
    assert!(result.is_err());
}

#[test]
fn test_dff_invalid_channel_count() {
    let data = vec![
        b'F', b'R', b'M', b'8', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, b'D', b'S', b'D',
        b' ', // FVER chunk
        b'F', b'V', b'E', b'R', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x01, 0x05, 0x00,
        0x00, // PROP chunk
        b'P', b'R', b'O', b'P', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2C, b'S', b'N', b'D',
        b' ', // FS chunk
        b'F', b'S', b' ', b' ', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x10, 0x2B,
        0x00, // CHNL chunk with invalid count
        b'C', b'H', b'N', b'L', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x00,
        0x07, // 7 channels (invalid, max is 6)
        b'S', b'L', b'F', b'T', b'S', b'R', b'G', b'T', // CMPR chunk
        b'C', b'M', b'P', b'R', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, b'D', b'S', b'D',
        b' ',
    ];

    let stream = create_stream(data);
    let result = DffReader::try_new(stream, Default::default());
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Byte-builder helpers
// ---------------------------------------------------------------------------

fn build_dsf(
    channel_type: u32,
    channels: u32,
    rate: u32,
    block: u32,
    sample_count: u64,
    data: &[u8],
) -> Vec<u8> {
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

fn be_u64(n: u64) -> [u8; 8] {
    n.to_be_bytes()
}

fn diin(title: &str, artist: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    for (id, txt) in [(b"DITI" as &[u8], title), (b"DIAR", artist)] {
        inner.extend_from_slice(id);
        inner.extend_from_slice(&be_u64(4 + txt.len() as u64));
        inner.extend_from_slice(&(txt.len() as u32).to_be_bytes());
        inner.extend_from_slice(txt.as_bytes());
        if txt.len() % 2 == 1 {
            inner.push(0);
        }
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
    if text.len() % 2 == 1 {
        body.push(0);
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"COMT");
    v.extend_from_slice(&be_u64(body.len() as u64));
    v.extend_from_slice(&body);
    v
}

fn build_dff(
    channels: u16,
    rate: u32,
    ch_ids: &[&[u8; 4]],
    data: &[u8],
    trailer: &[u8],
) -> Vec<u8> {
    build_dff_with(channels, rate, ch_ids, b"DSD ", &[], &chunk(b"DSD ", data), trailer)
}

/// Build a chunk: ID, 64-bit size, data, and a pad byte if the size is odd.
fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(id);
    v.extend_from_slice(&be_u64(data.len() as u64));
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// Build a DSDIFF file: FVER, PROP (with `extra_prop` sub-chunks appended), `sound` (a complete
/// DSD or DST chunk), then `trailer`.
fn build_dff_with(
    channels: u16,
    rate: u32,
    ch_ids: &[&[u8; 4]],
    cmpr: &[u8; 4],
    extra_prop: &[u8],
    sound: &[u8],
    trailer: &[u8],
) -> Vec<u8> {
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend(chunk(b"FS  ", &rate.to_be_bytes()));
    let mut chnl = channels.to_be_bytes().to_vec();
    for id in ch_ids {
        chnl.extend_from_slice(*id);
    }
    prop.extend(chunk(b"CHNL", &chnl));
    // CMPR: compression type and a (padded) Pascal string with its name.
    let mut cmpr_body = cmpr.to_vec();
    cmpr_body.extend_from_slice(&[0, 0]);
    prop.extend(chunk(b"CMPR", &cmpr_body));
    prop.extend_from_slice(extra_prop);
    let mut v = Vec::new();
    v.extend_from_slice(b"FRM8");
    v.extend_from_slice(&be_u64(0)); // patched below
    v.extend_from_slice(b"DSD ");
    v.extend(chunk(b"FVER", &0x0100_0000u32.to_be_bytes()));
    v.extend(chunk(b"PROP", &prop));
    v.extend_from_slice(sound);
    v.extend_from_slice(trailer);
    let total = v.len() as u64 - 12;
    v[4..12].copy_from_slice(&total.to_be_bytes());
    v
}

/// Build a DST sound data chunk. Each frame is a payload and an optional CRC (DSTC chunk).
fn build_dst(frame_rate: u16, frames: &[(&[u8], Option<u32>)], index: bool) -> Vec<u8> {
    let mut frte = (frames.len() as u32).to_be_bytes().to_vec();
    frte.extend_from_slice(&frame_rate.to_be_bytes());
    let mut body = chunk(b"FRTE", &frte);
    for (payload, crc) in frames {
        body.extend(chunk(b"DSTF", payload));
        if let Some(crc) = crc {
            body.extend(chunk(b"DSTC", &crc.to_be_bytes()));
        }
    }
    if index {
        body.extend(chunk(b"DSTI", &[0u8; 12]));
    }
    chunk(b"DST ", &body)
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

    let rev = reader.metadata().current().cloned().expect("expected a metadata revision");
    let tags = &rev.media.tags;

    let title_tag = tags.iter().find(|t| t.raw.key == "TITLE").expect("TITLE tag missing");
    assert!(
        format!("{}", title_tag.raw.value).contains("My Title"),
        "TITLE value did not contain 'My Title': {:?}",
        title_tag.raw.value
    );

    let artist_tag = tags.iter().find(|t| t.raw.key == "ARTIST").expect("ARTIST tag missing");
    assert!(
        format!("{}", artist_tag.raw.value).contains("My Artist"),
        "ARTIST value did not contain 'My Artist': {:?}",
        artist_tag.raw.value
    );

    let comment_tag = tags.iter().find(|t| t.raw.key == "COMMENT").expect("COMMENT tag missing");
    assert!(
        format!("{}", comment_tag.raw.value).contains("Hello"),
        "COMMENT value did not contain 'Hello': {:?}",
        comment_tag.raw.value
    );
}

// ---------------------------------------------------------------------------
// Padding, seeking, and robustness regression tests
// ---------------------------------------------------------------------------

/// DSF: the zero padding of the last block of each channel is not part of the packet data.
#[test]
fn test_dsf_trailing_padding_is_not_emitted() {
    // block=16 bytes per channel, 2 channels; 4 block groups. Channel 0 of block group g is filled
    // with 0xA0+g, and channel 1 with 0xB0+g. sample_count=500: the last block holds 500-3*128=116
    // DSD samples per channel = 15 bytes (14.5), then padding.
    let mut data = Vec::new();
    for g in 0..4u8 {
        data.extend_from_slice(&[0xA0 + g; 16]);
        data.extend_from_slice(&[0xB0 + g; 16]);
    }
    let raw = build_dsf(2, 2, 2822400, 16, 500, &data);
    let mut reader = DsfReader::try_new(create_stream(raw), Default::default()).unwrap();

    let mut packets = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        packets.push(p);
    }

    assert_eq!(packets.len(), 4);

    // Full blocks are not changed.
    for (g, p) in packets[..3].iter().enumerate() {
        assert_eq!(p.dur.get(), 128);
        assert_eq!(p.pts.get(), g as i64 * 128);
        assert_eq!(p.data.len(), 32);
    }

    // The last packet only contains the bytes of the valid samples of each channel (planar).
    let last = &packets[3];
    assert_eq!(last.dur.get(), 116);
    assert_eq!(last.pts.get(), 3 * 128);
    let mut expected = vec![0xA3u8; 15];
    expected.extend_from_slice(&[0xB3u8; 15]);
    assert_eq!(&last.data[..], &expected[..]);

    // The size of the data matches the number of samples: 8 DSD samples per byte per channel.
    let total_bytes_per_channel: usize = packets.iter().map(|p| p.data.len() / 2).sum();
    assert_eq!(total_bytes_per_channel, 500usize.div_ceil(8));
}

/// DSF: a sample count that is a multiple of the block size has no padding to trim.
#[test]
fn test_dsf_exact_blocks_are_not_trimmed() {
    let raw = build_dsf(2, 2, 2822400, 16, 256, &[0x55u8; 64]);
    let mut reader = DsfReader::try_new(create_stream(raw), Default::default()).unwrap();

    let mut n = 0;
    while let Some(p) = reader.next_packet().unwrap() {
        assert_eq!(p.data.len(), 32);
        assert_eq!(p.dur.get(), 128);
        n += 1;
    }
    assert_eq!(n, 2);
}

/// DSF/DFF: a zero sample rate is rejected rather than panicking.
#[test]
fn test_zero_sample_rate_is_rejected() {
    let raw = build_dsf(2, 2, 0, 16, 256, &[0u8; 64]);
    assert!(DsfReader::try_new(create_stream(raw), Default::default()).is_err());

    let raw = build_dff(2, 0, &[b"SLFT", b"SRGT"], &[0u8; 64], &[]);
    assert!(DffReader::try_new(create_stream(raw), Default::default()).is_err());
}

/// DSF: a zero block size is rejected, even if the rest of the file is well-formed.
#[test]
fn test_dsf_zero_block_size_with_data_is_rejected() {
    let raw = build_dsf(2, 2, 2822400, 0, 256, &[0u8; 64]);
    assert!(DsfReader::try_new(create_stream(raw), Default::default()).is_err());
}

/// DFF: seeking accounts for the byte-interleaved channels. Timestamps are in DSD samples per
/// channel, and 8 samples of one channel are a byte, so a stereo seek of N samples is N/8*2 bytes.
#[test]
fn test_dff_seek_is_in_samples_per_channel() {
    // 2 channels, interleaved: byte n of the data is n as u8.
    let data: Vec<u8> = (0..=255u8).collect();
    let raw = build_dff(2, 2822400, &[b"SLFT", b"SRGT"], &data, &[]);
    let mut reader = DffReader::try_new(create_stream(raw), Default::default()).unwrap();

    let result = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(803), track_id: 0 })
        .unwrap();

    // 803 / 8 = 100 bytes per channel (800 samples), so 200 bytes in.
    assert_eq!(result.required_ts.get(), 803);
    assert_eq!(result.actual_ts.get(), 800);

    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts.get(), 800);
    assert_eq!(packet.data[0], 200);
    // The first byte is for the first channel.
    assert_eq!(packet.data.len() % 2, 0);

    // Beyond the end of the data is out-of-range.
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1024), track_id: 0 })
            .is_err()
    );
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(-1), track_id: 0 })
            .is_err()
    );
}

/// DSF: the requested timestamp is reported as is.
#[test]
fn test_dsf_seek_reports_required_ts() {
    let raw = build_dsf(2, 2, 2822400, 16, 512, &[0u8; 128]);
    let mut reader = DsfReader::try_new(create_stream(raw), Default::default()).unwrap();

    let result = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(200), track_id: 0 })
        .unwrap();
    assert_eq!(result.required_ts.get(), 200);
    assert_eq!(result.actual_ts.get(), 128);

    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(-5), track_id: 0 })
            .is_err()
    );
}

/// The timeline of the tracks is in DSD samples per channel, at the DSD rate, for both formats.
#[test]
fn test_timeline_is_in_dsd_samples() {
    let raw = build_dsf(2, 2, 2822400, 4096, 2822400, &vec![0u8; 4096 * 2 * 87]);
    let reader = DsfReader::try_new(create_stream(raw), Default::default()).unwrap();
    let track = &reader.tracks()[0];
    let audio = track.codec_params.as_ref().unwrap().audio().unwrap();

    assert_eq!(audio.sample_rate, Some(2822400));
    assert_eq!(track.num_frames, Some(2822400));
    let tb = track.time_base.unwrap();
    assert_eq!(tb.calc_time(Timestamp::new(2822400)).unwrap().as_secs_f64(), 1.0);
}

// ---------------------------------------------------------------------------
// DSDIFF: DST, chunk order, truncation, metadata
// ---------------------------------------------------------------------------

use std::io::{Read, Seek, SeekFrom};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::Error;
use symphonia_core::io::MediaSource;
use symphonia_core::meta::StandardTag;
use symphonia_format_dsd::{CODEC_TYPE_DSD, CODEC_TYPE_DST};

const STEREO: &[&[u8; 4]] = &[b"SLFT", b"SRGT"];

fn dff_reader(raw: Vec<u8>) -> DffReader<'static> {
    DffReader::try_new(create_stream(raw), Default::default()).unwrap()
}

fn audio_of(reader: &DffReader<'_>) -> symphonia_core::codecs::audio::AudioCodecParameters {
    match reader.tracks()[0].codec_params.as_ref().unwrap() {
        CodecParameters::Audio(a) => a.clone(),
        _ => panic!("not an audio track"),
    }
}

/// An unknown compression type is reported as unsupported.
#[test]
fn test_dff_unknown_compression_is_unsupported() {
    let raw = build_dff_with(2, 2822400, STEREO, b"XXXX", &[], &chunk(b"DSD ", &[0u8; 8]), &[]);
    match DffReader::try_new(create_stream(raw), Default::default()) {
        Err(Error::Unsupported(msg)) => assert!(msg.contains("compression"), "{msg}"),
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("unknown compression accepted"),
    }
}

/// DST compression with no DST sound data chunk is an error that names DST.
#[test]
fn test_dff_dst_without_sound_data_is_an_error() {
    let raw = build_dff_with(2, 2822400, STEREO, b"DST ", &[], &[], &[]);
    let err = DffReader::try_new(create_stream(raw), Default::default()).err().unwrap();
    assert!(err.to_string().contains("DST"), "{err}");
}

/// DST frames are delivered one per packet with the frame CRC, and the timeline counts the DSD
/// samples per channel that each frame decodes to.
#[test]
fn test_dff_dst_frames_are_packets() {
    let frames: [(&[u8], Option<u32>); 4] = [
        (&[1, 2, 3], None),
        (&[4, 5, 6, 7], Some(0xDEAD_BEEF)),
        (&[8], None),
        (&[9, 10], Some(0x0102_0304)),
    ];
    let raw = build_dff_with(2, 2822400, STEREO, b"DST ", &[], &build_dst(75, &frames, true), &[]);
    let mut reader = dff_reader(raw);

    let audio = audio_of(&reader);
    assert_eq!(audio.codec, CODEC_TYPE_DST);
    assert_eq!(audio.sample_rate, Some(2822400));
    assert_eq!(audio.max_frames_per_packet, Some(37632));
    assert_eq!(audio.channel_data_layout, Some(ChannelDataLayout::Interleaved));
    assert_eq!(audio.bit_order, Some(BitOrder::MsbFirst));
    assert_eq!(reader.tracks()[0].num_frames, Some(4 * 37632));

    for (i, (payload, crc)) in frames.iter().enumerate() {
        let p = reader.next_packet().unwrap().expect("missing packet");
        assert_eq!(p.pts.get(), (i as i64) * 37632);
        assert_eq!(p.dur.get(), 37632);
        match crc {
            Some(crc) => {
                assert_eq!(p.data[0], 1);
                assert_eq!(p.data[1..5], crc.to_be_bytes());
            }
            None => assert_eq!(p.data[..5], [0u8; 5]),
        }
        assert_eq!(&p.data[5..], *payload);
    }
    assert!(reader.next_packet().unwrap().is_none());
}

/// Seeking in a DST file lands on the frame containing the timestamp, forwards (indexing frames
/// on the way) and backwards, and a seek past the last frame is out of range.
#[test]
fn test_dff_dst_seek() {
    let payloads: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 3 + usize::from(i)]).collect();
    let frames: Vec<(&[u8], Option<u32>)> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| (&p[..], (i % 3 == 0).then_some(i as u32)))
        .collect();
    let raw = build_dff_with(2, 2822400, STEREO, b"DST ", &[], &build_dst(75, &frames, true), &[]);
    let mut reader = dff_reader(raw);

    // Straight to a frame that has not been indexed yet.
    let to = SeekTo::Timestamp { ts: Timestamp::new(17 * 37632 + 5), track_id: 0 };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!(seeked.actual_ts.get(), 17 * 37632);
    assert_eq!(seeked.required_ts.get(), 17 * 37632 + 5);
    let p = reader.next_packet().unwrap().unwrap();
    assert_eq!(p.pts.get(), 17 * 37632);
    assert_eq!(&p.data[5..], &payloads[17][..]);

    // Backwards, and then sequentially from there.
    let to = SeekTo::Timestamp { ts: Timestamp::new(2 * 37632), track_id: 0 };
    assert_eq!(reader.seek(SeekMode::Accurate, to).unwrap().actual_ts.get(), 2 * 37632);
    let p = reader.next_packet().unwrap().unwrap();
    assert_eq!(&p.data[5..], &payloads[2][..]);
    let p = reader.next_packet().unwrap().unwrap();
    assert_eq!(p.pts.get(), 3 * 37632);
    assert_eq!(p.data[0], 1, "frame 3 has a CRC");
    assert_eq!(&p.data[5..], &payloads[3][..]);

    // The last frame, then past the end.
    let to = SeekTo::Timestamp { ts: Timestamp::new(19 * 37632), track_id: 0 };
    reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 19 * 37632);
    assert!(reader.next_packet().unwrap().is_none());
    let to = SeekTo::Timestamp { ts: Timestamp::new(20 * 37632), track_id: 0 };
    assert!(matches!(
        reader.seek(SeekMode::Accurate, to),
        Err(Error::SeekError(symphonia_core::errors::SeekErrorKind::OutOfRange))
    ));
}

/// A DST file that ends in the middle of a frame delivers the complete frames only.
#[test]
fn test_dff_dst_truncated_file() {
    let frames: [(&[u8], Option<u32>); 3] =
        [(&[1; 100], None), (&[2; 100], Some(7)), (&[3; 100], None)];
    let mut raw =
        build_dff_with(2, 2822400, STEREO, b"DST ", &[], &build_dst(75, &frames, false), &[]);
    raw.truncate(raw.len() - 60);
    let mut reader = dff_reader(raw);

    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0);
    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 37632);
    assert!(reader.next_packet().unwrap().is_none());
}

/// A DST frame rate that does not divide the sample rate in whole bytes is rejected.
#[test]
fn test_dff_dst_bad_frame_rate_is_rejected() {
    let frames: [(&[u8], Option<u32>); 1] = [(&[1], None)];
    // 2822400 / 74 is not whole, and 2822400 / 64 = 44100 is not a whole number of bytes.
    for rate in [0u16, 74, 64] {
        let raw =
            build_dff_with(2, 2822400, STEREO, b"DST ", &[], &build_dst(rate, &frames, false), &[]);
        assert!(DffReader::try_new(create_stream(raw), Default::default()).is_err(), "{rate}");
    }
}

/// A DSD chunk that claims more data than the file holds delivers the complete samples that are
/// present, and the track is as long as that data.
#[test]
fn test_dff_truncated_data_chunk() {
    let data: Vec<u8> = (0..=255u8).cycle().take(10_001).collect();
    let mut sound = Vec::new();
    sound.extend_from_slice(b"DSD ");
    sound.extend_from_slice(&be_u64(1_000_000)); // claims far more than is present
    sound.extend_from_slice(&data);
    let raw = build_dff_with(2, 2822400, STEREO, b"DSD ", &[], &sound, &[]);
    let mut reader = dff_reader(raw);

    // 10001 bytes, two channels: 5000 complete samples groups = 40000 DSD samples per channel.
    assert_eq!(reader.tracks()[0].num_frames, Some(5000 * 8));
    let mut got = Vec::new();
    let mut dur = 0;
    while let Some(p) = reader.next_packet().unwrap() {
        assert_eq!(p.pts.get(), dur);
        dur += p.dur.get() as i64;
        got.extend_from_slice(&p.data);
    }
    assert_eq!(got, &data[..10_000]);
    assert_eq!(dur, 40_000);
}

/// The chunks may come in any order, unknown chunks are skipped, and metadata after the sound
/// data is found. The FRM8 size is not trusted.
#[test]
fn test_dff_chunk_order_and_unknown_chunks() {
    let data = [0x55u8; 64];
    let mut trailer = chunk(b"JUNK", &[1, 2, 3]);
    trailer.extend(diin("Title", "Artist"));
    trailer.extend(comt("Remark"));
    let mut raw = build_dff(2, 2822400, STEREO, &data, &trailer);
    // Insert metadata and an unknown chunk before the PROP chunk too.
    let prop_at = raw.windows(4).position(|w| w == b"PROP").unwrap();
    let mut pre = chunk(b"MANF", b"ABCDxyz");
    pre.extend(chunk(b"ZZZZ", &[0; 5]));
    raw.splice(prop_at..prop_at, pre);
    // A wrong FRM8 size.
    raw[4..12].copy_from_slice(&be_u64(10));

    let mut reader = dff_reader(raw);
    assert_eq!(reader.tracks()[0].num_frames, Some(256));
    let rev = reader.metadata().current().cloned().expect("metadata");
    let keys: Vec<&str> = rev.media.tags.iter().map(|t| t.raw.key.as_str()).collect();
    for key in ["TITLE", "ARTIST", "COMMENT", "MANUFACTURER"] {
        assert!(keys.contains(&key), "{key} missing from {keys:?}");
    }
    let title = rev.media.tags.iter().find(|t| t.raw.key == "TITLE").unwrap();
    assert!(matches!(&title.std, Some(StandardTag::TrackTitle(t)) if t.as_str() == "Title"));
    let comment = rev.media.tags.iter().find(|t| t.raw.key == "COMMENT").unwrap();
    assert!(matches!(&comment.std, Some(StandardTag::Comment(c)) if c.as_str() == "Remark"));

    let mut bytes = 0;
    while let Some(p) = reader.next_packet().unwrap() {
        bytes += p.data.len();
    }
    assert_eq!(bytes, 64);
}

/// An ID3v2 tag in an `ID3 ` chunk after the sound data is read.
#[test]
fn test_dff_id3_chunk() {
    // ID3v2.3 with a TIT2 frame.
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    let frame_body = [&[0u8][..], b"Id3 Title"].concat();
    let mut frame = b"TIT2".to_vec();
    frame.extend_from_slice(&(frame_body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&frame_body);
    let size = frame.len() as u32;
    tag.extend_from_slice(&[
        (size >> 21) as u8 & 0x7F,
        (size >> 14) as u8 & 0x7F,
        (size >> 7) as u8 & 0x7F,
        size as u8 & 0x7F,
    ]);
    tag.extend_from_slice(&frame);

    let raw = build_dff(2, 2822400, STEREO, &[0u8; 32], &chunk(b"ID3 ", &tag));
    let mut reader = dff_reader(raw);
    let revisions: Vec<_> = {
        let mut v = Vec::new();
        let mut md = reader.metadata();
        while let Some(rev) = md.current().cloned() {
            v.push(rev);
            if md.pop().is_none() {
                break;
            }
        }
        v
    };
    let found = revisions
        .iter()
        .flat_map(|r| r.media.tags.iter())
        .any(|t| matches!(&t.std, Some(StandardTag::TrackTitle(v)) if v.as_str() == "Id3 Title"));
    assert!(found, "ID3 title missing");
}

/// Markers become chapters on the timeline of the sound data (relative to ABSS), and a track
/// start ends at the next track start or stop.
#[test]
fn test_dff_markers_become_chapters() {
    fn mark(secs: u8, ty: u16, text: &str) -> Vec<u8> {
        let mut b = 0u16.to_be_bytes().to_vec(); // hours
        b.extend_from_slice(&[0, secs]); // minutes, seconds
        b.extend_from_slice(&0u32.to_be_bytes()); // samples
        b.extend_from_slice(&0i32.to_be_bytes()); // offset
        b.extend_from_slice(&ty.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes()); // channel
        b.extend_from_slice(&0u16.to_be_bytes()); // track flags
        b.extend_from_slice(&(text.len() as u32).to_be_bytes());
        b.extend_from_slice(text.as_bytes());
        chunk(b"MARK", &b)
    }
    let mut diin = mark(12, 0, "First");
    diin.extend(mark(20, 0, "Second"));
    diin.extend(mark(30, 1, ""));
    let trailer = chunk(b"DIIN", &diin);
    // ABSS: 10 seconds.
    let mut abss = 0u16.to_be_bytes().to_vec();
    abss.extend_from_slice(&[0, 10]);
    abss.extend_from_slice(&0u32.to_be_bytes());
    let raw = build_dff_with(
        2,
        2822400,
        STEREO,
        b"DSD ",
        &chunk(b"ABSS", &abss),
        &chunk(b"DSD ", &[0u8; 64]),
        &trailer,
    );
    let reader = dff_reader(raw);

    let group = reader.chapters().expect("chapters");
    let chapters: Vec<_> = group
        .items
        .iter()
        .filter_map(|i| match i {
            symphonia_core::meta::ChapterGroupItem::Chapter(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(chapters.len(), 3);
    assert_eq!(chapters[0].start_time.as_secs(), 2);
    assert_eq!(chapters[0].end_time.unwrap().as_secs(), 10);
    assert_eq!(chapters[1].start_time.as_secs(), 10);
    assert_eq!(chapters[1].end_time.unwrap().as_secs(), 20);
    assert_eq!(chapters[2].start_time.as_secs(), 20);
    let title = &chapters[0].tags.iter().find(|t| t.raw.key == "TITLE").unwrap();
    assert!(matches!(&title.std, Some(StandardTag::ChapterTitle(t)) if t.as_str() == "First"));
}

/// Context-specific channel IDs are resolved through the loudspeaker configuration.
#[test]
fn test_dff_loudspeaker_configuration_names_channels() {
    let mut lsco = Vec::new();
    lsco.extend_from_slice(&4u16.to_be_bytes());
    let ids: [&[u8; 4]; 6] = [b"C000", b"C001", b"C002", b"C003", b"C004", b"C005"];
    let raw = build_dff_with(
        6,
        2822400,
        &ids,
        b"DSD ",
        &chunk(b"LSCO", &lsco),
        &chunk(b"DSD ", &[0u8; 60]),
        &[],
    );
    let reader = dff_reader(raw);
    assert_eq!(
        audio_of(&reader).channels,
        Some(Channels::Positioned(
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        ))
    );

    // Without it the channels are discrete.
    let raw = build_dff_with(6, 2822400, &ids, b"DSD ", &[], &chunk(b"DSD ", &[0u8; 60]), &[]);
    assert_eq!(audio_of(&dff_reader(raw)).channels, Some(Channels::Discrete(6)));
}

/// A source that is not seekable.
struct Unseekable(std::io::Cursor<Vec<u8>>);

impl Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for Unseekable {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::other("unseekable"))
    }
}

impl MediaSource for Unseekable {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Files can be played from a stream that cannot seek, as long as the metadata that matters comes
/// before the sound data. Seeking reports that it is not possible.
#[test]
fn test_dff_unseekable_stream() {
    let data: Vec<u8> = (0..200u8).collect();
    let raw = build_dff(2, 2822400, STEREO, &data, &comt("late"));
    let stream = MediaSourceStream::new(Box::new(Unseekable(Cursor::new(raw))), Default::default());
    let mut reader = DffReader::try_new(stream, Default::default()).unwrap();
    assert_eq!(reader.tracks()[0].num_frames, Some(800));

    let mut got = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        got.extend_from_slice(&p.data);
    }
    assert_eq!(got, data);
    let to = SeekTo::Timestamp { ts: Timestamp::new(8), track_id: 0 };
    assert!(matches!(
        reader.seek(SeekMode::Accurate, to),
        Err(Error::SeekError(symphonia_core::errors::SeekErrorKind::Unseekable))
    ));
}

/// The codec ID of uncompressed DSDIFF is DSD.
#[test]
fn test_dff_dsd_codec() {
    let reader = dff_reader(build_dff(2, 2822400, STEREO, &[0u8; 16], &[]));
    assert_eq!(audio_of(&reader).codec, CODEC_TYPE_DSD);
}
