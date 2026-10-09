// Mapping family 3 (RFC 8486 ambisonics with a demixing matrix, libopus `opus_projection_decoder`).
//
// The fixtures in `tests/data` are 20 ms CELT streams written by libopus 1.6.1's projection
// encoder (`opus_projection_ambisonics_encoder_create`, family 3; first order = 4 channels,
// second order = 9 channels with an odd number of streams, third order = 16 channels). The
// `*.golden.f32` files are `opus_projection_decode_float` output of the same libopus for the
// frames 1920..2400 of the stream (decoded from a cold start, pre-skip included).
//
// Set `OPUS_FAMILY3_REF=<dir>` to additionally compare the whole stream against full
// `<name>.ref.f32` decodes (`family3_<name>.ref.f32`) produced by libopus.

use symphonia_codec_opus::decoder::SampleRate;
use symphonia_codec_opus::mapping::{ChannelMapping, MappingError, OpusHead};
use symphonia_codec_opus::multistream::MultistreamDecoder;

const MAX_FRAME: usize = 5760;

/// Splits an Ogg stream into packets (the fixtures only contain whole packets per page).
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut cur = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        assert_eq!(&data[pos..pos + 4], b"OggS");
        let nseg = data[pos + 26] as usize;
        let segs = &data[pos + 27..pos + 27 + nseg];
        let mut off = pos + 27 + nseg;
        for &s in segs {
            cur.extend_from_slice(&data[off..off + s as usize]);
            off += s as usize;
            if s < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        pos = off;
    }
    packets
}

fn fixture(name: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut packets = ogg_packets(&data);
    let head = packets.remove(0);
    packets.remove(0); // OpusTags
    (head, packets)
}

fn read_f32(path: &std::path::Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

fn decode_all(dec: &mut MultistreamDecoder, packets: &[Vec<u8>], channels: usize) -> Vec<f32> {
    let mut out = Vec::new();
    let mut scratch = vec![0f32; MAX_FRAME * channels];
    for p in packets {
        let n = dec.decode(Some(p), &mut scratch, MAX_FRAME, false).expect("decode");
        out.extend_from_slice(&scratch[..n * channels]);
    }
    out
}

fn max_err(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()))
}

const FIXTURES: [(&str, usize, u8, u8); 3] = [("foa", 4, 2, 2), ("o2", 9, 5, 4), ("o3", 16, 8, 8)];

#[test]
fn family3_header_is_parsed() {
    for (name, channels, streams, coupled) in FIXTURES {
        let (head, _) = fixture(&format!("family3_{name}.opus"));
        let h = OpusHead::parse(&head).unwrap();
        assert_eq!(usize::from(h.channel_count), channels);
        assert_eq!(h.mapping.family, 3);
        assert_eq!(h.mapping.stream_count, streams);
        assert_eq!(h.mapping.coupled_count, coupled);
        assert!(h.mapping.table.is_empty());
        let n_in = usize::from(streams) + usize::from(coupled);
        assert_eq!(h.mapping.demixing_matrix.len(), channels * n_in);
        // The matrix is stored little-endian, column-major.
        let bytes = &head[21..21 + 2 * channels * n_in];
        assert_eq!(h.mapping.demixing_matrix[1], i16::from_le_bytes([bytes[2], bytes[3]]));
        // libopus' ambisonics demixing matrices are not trivial.
        assert!(h.mapping.demixing_matrix.iter().any(|&m| m != 0));
    }
}

#[test]
fn family3_header_validation() {
    let (head, _) = fixture("family3_foa.opus");

    // Truncated matrix.
    assert_eq!(OpusHead::parse(&head[..head.len() - 1]), Err(MappingError::InvalidHeader));
    assert_eq!(OpusHead::parse(&head[..20]), Err(MappingError::InvalidHeader));

    // Zero streams / more coupled than streams.
    let mut bad = head.clone();
    bad[19] = 0;
    assert_eq!(OpusHead::parse(&bad), Err(MappingError::InvalidMapping));
    let mut bad = head.clone();
    bad[20] = 3;
    assert_eq!(OpusHead::parse(&bad), Err(MappingError::InvalidMapping));

    // More output channels than decoded channels (libopus' trivial mapping cannot address them).
    let mut bad = head.clone();
    bad[9] = 5;
    bad.resize(head.len() + 2 * 4, 0);
    assert_eq!(OpusHead::parse(&bad), Err(MappingError::InvalidMapping));

    // A well-formed header whose decoder layout is inconsistent is rejected by the decoder too.
    let h = OpusHead::parse(&head).unwrap();
    let mut mapping = h.mapping.clone();
    mapping.demixing_matrix.pop();
    assert!(MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, mapping).is_err());
    let mut mapping = h.mapping.clone();
    mapping.demixing_matrix.clear();
    assert!(MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, mapping).is_err());
}

#[test]
fn family3_matches_libopus() {
    for (name, channels, _, _) in FIXTURES {
        let (head, packets) = fixture(&format!("family3_{name}.opus"));
        let h = OpusHead::parse(&head).unwrap();
        let mut dec =
            MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, h.mapping.clone())
                .unwrap();
        dec.set_gain(h.output_gain);
        let pcm = decode_all(&mut dec, &packets, channels);
        assert_eq!(pcm.len() % channels, 0);

        let golden = read_f32(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/data/family3_{name}.golden.f32")),
        );
        let got = &pcm[1920 * channels..2400 * channels];
        let err = max_err(got, &golden);
        println!("family3 {name}: max error vs libopus over the golden frames = {err:e}");
        assert!(err < 1e-6, "{name}: max error {err}");

        if let Some(dir) = std::env::var_os("OPUS_FAMILY3_REF") {
            let reference =
                read_f32(&std::path::Path::new(&dir).join(format!("f3_{name}.ref.f32")));
            let err = max_err(&pcm, &reference);
            println!(
                "family3 {name}: max error vs full libopus decode = {err:e} ({} frames)",
                pcm.len() / channels
            );
            assert!(err < 1e-6, "{name}: max error {err}");
        }
    }
}

/// An independent (f64) implementation of the demixing: decodes the same packets as plain
/// multistream (family 255, identity table) and applies the matrix to the decoded channels.
#[test]
fn family3_equals_independent_matrix_product() {
    for (name, channels, streams, coupled) in FIXTURES {
        let (head, packets) = fixture(&format!("family3_{name}.opus"));
        let h = OpusHead::parse(&head).unwrap();
        let n_in = usize::from(streams) + usize::from(coupled);

        let mut proj =
            MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, h.mapping.clone())
                .unwrap();
        let projected = decode_all(&mut proj, &packets, channels);

        let plain_mapping = ChannelMapping {
            family: 255,
            stream_count: streams,
            coupled_count: coupled,
            table: (0..n_in as u8).collect(),
            demixing_matrix: Vec::new(),
        };
        let mut plain =
            MultistreamDecoder::try_new(SampleRate::Hz48000, n_in as u8, plain_mapping).unwrap();
        let decoded = decode_all(&mut plain, &packets, n_in);
        assert_eq!(decoded.len() / n_in, projected.len() / channels);

        let m = &h.mapping.demixing_matrix;
        let mut worst = 0f64;
        for (frame, out) in decoded.chunks_exact(n_in).zip(projected.chunks_exact(channels)) {
            for row in 0..channels {
                let expect: f64 = (0..n_in)
                    .map(|col| f64::from(m[col * channels + row]) / 32768.0 * f64::from(frame[col]))
                    .sum();
                worst = worst.max((expect - f64::from(out[row])).abs());
            }
        }
        println!("family3 {name}: max error vs f64 matrix product = {worst:e}");
        assert!(worst < 1e-6, "{name}: {worst}");
    }
}

/// A synthetic matrix: output channel `r` is `0.5 * decoded[n - 1 - r]` (a reversal with a
/// gain of exactly 0.5 in Q15), which must be bit-exact and exercises the column-major layout.
#[test]
fn family3_column_major_permutation_is_exact() {
    let (head, packets) = fixture("family3_foa.opus");
    let h = OpusHead::parse(&head).unwrap();
    let n = usize::from(h.channel_count);

    let mut matrix = vec![0i16; n * n];
    for col in 0..n {
        let row = n - 1 - col;
        matrix[col * n + row] = 16384;
    }
    let mapping = ChannelMapping { demixing_matrix: matrix, ..h.mapping.clone() };
    let mut proj =
        MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, mapping).unwrap();
    let projected = decode_all(&mut proj, &packets, n);

    let plain_mapping = ChannelMapping {
        family: 255,
        stream_count: h.mapping.stream_count,
        coupled_count: h.mapping.coupled_count,
        table: (0..n as u8).collect(),
        demixing_matrix: Vec::new(),
    };
    let mut plain =
        MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, plain_mapping).unwrap();
    let decoded = decode_all(&mut plain, &packets, n);

    for (frame, out) in decoded.chunks_exact(n).zip(projected.chunks_exact(n)) {
        for r in 0..n {
            assert_eq!(out[r].to_bits(), (0.5 * frame[n - 1 - r]).to_bits());
        }
    }
}

/// Lost packets (PLC) go through the same demixing path and keep the frame count.
#[test]
fn family3_packet_loss_concealment() {
    let (head, packets) = fixture("family3_foa.opus");
    let h = OpusHead::parse(&head).unwrap();
    let n = usize::from(h.channel_count);
    let mut dec =
        MultistreamDecoder::try_new(SampleRate::Hz48000, h.channel_count, h.mapping.clone())
            .unwrap();
    let mut out = vec![0f32; MAX_FRAME * n];
    for p in &packets[..5] {
        dec.decode(Some(p), &mut out, MAX_FRAME, false).unwrap();
    }
    let frames = dec.decode(None, &mut out, 960, false).unwrap();
    assert_eq!(frames, 960);
    assert!(out[..960 * n].iter().all(|v| v.is_finite()));
    assert!(out[..960 * n].iter().any(|&v| v != 0.0));
}
