#![allow(clippy::unwrap_used)]

use std::io::Cursor;
use std::path::Path;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::CODEC_ID_NULL_AUDIO;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_EVRC, CODEC_ID_QCELP};
use symphonia_core::errors::Error;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_format_qcp::QcpReader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn open(bytes: Vec<u8>) -> Result<Box<dyn FormatReader + 'static>, Error> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut probe = Probe::default();
    probe.register_format::<QcpReader<'_>>();
    probe.probe(&Hint::new(), mss, Default::default(), Default::default())
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(FIXTURES).join(name)).unwrap()
}

/// Builds a QCP file. The rate map is for QCELP: payload sizes 34, 16, 7, 3 for the modes 4 to 1.
fn build(
    guid: [u8; 16],
    packet_size: u16,
    vrat: Option<(u32, u32)>,
    rates: &[(u8, u8)],
    data: &[u8],
) -> Vec<u8> {
    let mut fmt = vec![1, 0];
    fmt.extend_from_slice(&guid);
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&[0u8; 80]);
    fmt.extend_from_slice(&13000u16.to_le_bytes());
    fmt.extend_from_slice(&packet_size.to_le_bytes());
    fmt.extend_from_slice(&160u16.to_le_bytes());
    fmt.extend_from_slice(&8000u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&(rates.len() as u32).to_le_bytes());
    for &(size, mode) in rates {
        fmt.extend_from_slice(&[size, mode]);
    }
    fmt.resize(150, 0);

    let mut body = b"QLCM".to_vec();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&150u32.to_le_bytes());
    body.extend_from_slice(&fmt);
    if let Some((flag, count)) = vrat {
        body.extend_from_slice(b"vrat");
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&flag.to_le_bytes());
        body.extend_from_slice(&count.to_le_bytes());
    }
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);
    if data.len() % 2 == 1 {
        body.push(0);
    }

    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend_from_slice(&body);
    file
}

const QCELP_GUID: [u8; 16] = [
    0x41, 0x6d, 0x7f, 0x5e, 0x15, 0xb1, 0xd0, 0x11, 0xba, 0x91, 0x00, 0x80, 0x5f, 0xb4, 0xb9, 0x7e,
];
const SMV_GUID: [u8; 16] = [
    0x75, 0x2b, 0x7c, 0x8d, 0x97, 0xa7, 0x49, 0xed, 0x98, 0x5e, 0xd5, 0x3c, 0x8c, 0xc7, 0x5f, 0x84,
];

const QCELP_RATES: [(u8, u8); 5] = [(34, 4), (16, 3), (7, 2), (3, 1), (0, 0)];

fn packets(reader: &mut dyn FormatReader) -> Vec<(i64, Vec<u8>)> {
    let mut out = vec![];
    while let Some(p) = reader.next_packet().unwrap() {
        assert_eq!(p.dur.get(), 160);
        out.push((p.pts.get(), p.data.to_vec()));
    }
    out
}

#[test]
fn verify_variable_rate_fixture() {
    let mut reader = open(fixture("clap.qcp")).unwrap();
    assert_eq!(reader.format_info().short_name, "qcp");

    let track = reader.default_track(TrackType::Audio).unwrap();
    assert_eq!(track.num_frames, Some(175 * 160));
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };
    assert_eq!(params.codec, CODEC_ID_QCELP);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1));

    let pkts = packets(reader.as_mut());
    assert_eq!(pkts.len(), 175);
    for (i, (pts, data)) in pkts.iter().enumerate() {
        assert_eq!(*pts, i as i64 * 160);
        // The rate byte, then the payload of the rate.
        let expected = match data[0] {
            1 => 4,
            2 => 8,
            3 => 17,
            4 => 35,
            m => panic!("unexpected rate byte {m}"),
        };
        assert_eq!(data.len(), expected);
    }
}

#[test]
fn verify_fixed_size_fixture() {
    let mut reader = open(fixture("test8.qcp")).unwrap();
    let pkts = packets(reader.as_mut());
    assert_eq!(pkts.len(), 400);
    assert!(pkts.iter().all(|(_, d)| d.len() == 35));
}

#[test]
fn verify_evrc_fixture() {
    let mut reader = open(fixture("evrc.qcp")).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };
    assert_eq!(params.codec, CODEC_ID_EVRC);

    // The count of packets of `ffprobe -count_packets`.
    assert_eq!(packets(reader.as_mut()).len(), 15184);
}

#[test]
fn verify_seek() {
    for (name, last) in [("clap.qcp", 174u64), ("test8.qcp", 399)] {
        let all = packets(open(fixture(name)).unwrap().as_mut());

        let mut reader = open(fixture(name)).unwrap();
        // Forward, past the packets read so far, then back, then to the end.
        for target in [100u64, 3, 3, last, 0, 57] {
            let ts = Timestamp::new((target * 160 + 17) as i64);
            let seeked =
                reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: 0 }).unwrap();
            assert_eq!(seeked.actual_ts.get(), (target * 160) as i64, "{name}: {target}");

            let p = reader.next_packet().unwrap().unwrap();
            assert_eq!(p.pts.get(), (target * 160) as i64);
            assert_eq!(&*p.data, &all[target as usize].1[..], "{name}: {target}");
        }

        let ts = Timestamp::new(((last + 1) * 160) as i64);
        assert!(reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: 0 }).is_err());
    }
}

#[test]
fn verify_fixed_size_ignores_rate_byte() {
    // Fixed packets of 6 bytes: the rate byte is not used to find the size.
    let data = [4, 1, 2, 3, 4, 5, 2, 6, 7, 8, 9, 10, 1, 0, 0, 0, 0, 0];
    let mut reader = open(build(QCELP_GUID, 6, None, &QCELP_RATES, &data)).unwrap();
    let pkts = packets(reader.as_mut());
    assert_eq!(pkts.len(), 3);
    assert_eq!(pkts[1].1, [2, 6, 7, 8, 9, 10]);
}

#[test]
fn verify_variable_rate_skips_unknown_rate_bytes() {
    // Rate bytes 9 and 0 (0 has no payload, so its packet is the rate byte alone).
    let data = [1, 0xa, 0xb, 0xc, 9, 0, 1, 0xd, 0xe, 0xf];
    let mut reader = open(build(QCELP_GUID, 0, Some((1, 3)), &QCELP_RATES, &data)).unwrap();
    let pkts = packets(reader.as_mut());
    let sizes: Vec<_> = pkts.iter().map(|(_, d)| d.len()).collect();
    assert_eq!(sizes, [4, 1, 4]);
}

#[test]
fn verify_truncated_data() {
    // The last packet is cut short: it is returned as is.
    let data = [4u8; 35 + 10];
    let mut reader = open(build(QCELP_GUID, 0, Some((1, 2)), &QCELP_RATES, &data)).unwrap();
    let pkts = packets(reader.as_mut());
    assert_eq!(pkts.len(), 2);
    assert_eq!(pkts[1].1.len(), 10);
}

#[test]
fn verify_smv_has_no_codec() {
    let data = [4u8; 35];
    let reader = open(build(SMV_GUID, 35, None, &QCELP_RATES, &data)).unwrap();
    let Some(CodecParameters::Audio(params)) = &reader.tracks()[0].codec_params
    else {
        panic!("no audio parameters");
    };
    assert_eq!(params.codec, CODEC_ID_NULL_AUDIO);
}

#[test]
fn verify_rejects_malformed() {
    let good = build(QCELP_GUID, 35, None, &QCELP_RATES, &[4u8; 70]);
    assert!(open(good.clone()).is_ok());

    // Unknown GUID.
    let mut bad = good.clone();
    bad[12 + 8 + 2] ^= 0xff;
    assert!(open(bad).is_err());

    // Every truncation of the header, and arbitrary corruptions, must not panic.
    for len in 0..220 {
        let _ = open(good[..len.min(good.len())].to_vec())
            .map(|mut r| while let Ok(Some(_)) = r.next_packet() {});
    }
    let mut state = 1u32;
    for _ in 0..3000 {
        let mut bad = good.clone();
        for _ in 0..3 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let at = (state >> 8) as usize % bad.len();
            bad[at] = (state >> 24) as u8;
        }
        if let Ok(mut r) = open(bad) {
            while let Ok(Some(_)) = r.next_packet() {}
        }
    }
}
