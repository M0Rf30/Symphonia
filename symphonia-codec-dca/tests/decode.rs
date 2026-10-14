// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::excessive_precision, clippy::unwrap_used)]

//! Decodes three frames of a 48 kHz 5.1 core stream (encoded from synthetic sine tones) and
//! compares samples against the output of a reference decoder.

use symphonia_codec_dca::DcaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef, Position};
use symphonia_core::codecs::audio::well_known::CODEC_ID_DCA;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

const STREAM: &[u8] = include_bytes!("data/sine_5_1.dts");
const FRAME_SIZE: usize = 1884;
const FRAME_SAMPLES: usize = 512;

const SAMPLE_IDX: [usize; 11] = [100, 231, 362, 493, 624, 755, 886, 1017, 1148, 1279, 1410];

// Reference samples in the order FL, FR, FC, LFE1, RL, RR.
static EXPECTED: [[f32; 11]; 6] = [
    [
        -3.46240103e-09,
        5.30766374e-06,
        1.55410930e-06,
        1.98645762e-06,
        2.08320301e-02,
        1.23716637e-01,
        5.44191264e-02,
        -9.06464979e-02,
        -1.09548755e-01,
        2.40453295e-02,
        1.24160014e-01,
    ],
    [
        7.85301868e-09,
        -4.90011814e-07,
        7.43870487e-06,
        -4.73966247e-05,
        -3.10697295e-02,
        1.04981579e-01,
        9.75375846e-02,
        -4.32717986e-02,
        -1.24907002e-01,
        -3.58018130e-02,
        1.02262303e-01,
    ],
    [
        1.88281057e-09,
        -5.47533728e-06,
        2.30404785e-05,
        -2.61802488e-05,
        4.10888046e-02,
        3.48740481e-02,
        -9.79430377e-02,
        1.24833345e-01,
        -1.05512761e-01,
        4.72160168e-02,
        2.85653789e-02,
    ],
    [
        0.00000000e+00,
        4.98155423e-05,
        2.51853629e-03,
        2.32902877e-02,
        6.60554916e-02,
        3.83263044e-02,
        -3.81142413e-03,
        -3.59317660e-02,
        -7.81377703e-02,
        -3.48406844e-02,
        8.51986371e-03,
    ],
    [
        7.98668598e-09,
        7.33647880e-07,
        -1.53723358e-05,
        -2.86248883e-06,
        6.02107421e-02,
        -1.13903895e-01,
        1.21993437e-01,
        -8.11840594e-02,
        7.84249790e-03,
        6.86126873e-02,
        -1.17597602e-01,
    ],
    [
        -5.71001513e-09,
        -4.94386131e-06,
        -2.95448135e-05,
        -5.51678859e-05,
        -1.08216770e-01,
        8.83825049e-02,
        -6.24739975e-02,
        3.23483460e-02,
        -2.57183302e-07,
        -3.23485844e-02,
        6.25035837e-02,
    ],
];

fn new_decoder() -> DcaDecoder {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DCA);
    DcaDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
}

fn decode_packet(dec: &mut DcaDecoder, data: &[u8]) -> Option<Vec<Vec<f32>>> {
    let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);

    match dec.decode_ref(&packet) {
        Ok(GenericAudioBufferRef::F32(buf)) => {
            let n = buf.spec().channels().count();
            Some((0..n).map(|c| buf.plane(c).unwrap().to_vec()).collect())
        }
        Ok(_) => panic!("expected f32 samples"),
        Err(_) => None,
    }
}

/// Decode `packets` and return planar PCM.
fn decode_all(packets: &[Vec<u8>]) -> Vec<Vec<f32>> {
    let mut dec = new_decoder();
    let mut pcm: Vec<Vec<f32>> = vec![Vec::new(); 6];

    for p in packets {
        let planes = decode_packet(&mut dec, p).expect("packet should decode");
        assert_eq!(planes.len(), 6);
        for (dst, src) in pcm.iter_mut().zip(&planes) {
            dst.extend_from_slice(src);
        }
    }

    pcm
}

fn core_packets() -> Vec<Vec<u8>> {
    STREAM.chunks(FRAME_SIZE).map(<[u8]>::to_vec).collect()
}

fn check(pcm: &[Vec<f32>]) {
    for (ch, plane) in pcm.iter().enumerate() {
        assert_eq!(plane.len(), 3 * FRAME_SAMPLES);
        for (i, &idx) in SAMPLE_IDX.iter().enumerate() {
            let diff = (plane[idx] - EXPECTED[ch][i]).abs();
            assert!(diff < 2e-6, "ch {ch} sample {idx}: {} vs {}", plane[idx], EXPECTED[ch][i]);
        }
    }
}

/// Convert a frame to the 16-bit little-endian bitstream form.
fn to_le16(frame: &[u8]) -> Vec<u8> {
    frame.chunks(2).flat_map(|w| [w[1], w[0]]).collect()
}

/// Convert a frame to the 14-bit bitstream form (big or little-endian words).
fn to_14bit(frame: &[u8], little_endian: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut nbits = 0;

    let mut emit = |acc: u32| {
        let mut w = (acc & 0x3fff) as u16;
        // The two most significant bits of a word replicate the sign bit of the 14-bit value.
        if w & 0x2000 != 0 {
            w |= 0xc000;
        }
        if little_endian {
            out.extend_from_slice(&w.to_le_bytes())
        }
        else {
            out.extend_from_slice(&w.to_be_bytes())
        }
    };

    for &b in frame {
        acc = acc << 8 | u32::from(b);
        nbits += 8;
        if nbits >= 14 {
            nbits -= 14;
            emit(acc >> nbits);
            acc &= (1 << nbits) - 1;
        }
    }
    if nbits > 0 {
        emit(acc << (14 - nbits));
    }

    out
}

#[test]
fn decodes_5_1_core() {
    let mut dec = new_decoder();
    let packets = core_packets();

    let first = decode_packet(&mut dec, &packets[0]).unwrap();
    assert_eq!(first[0].len(), FRAME_SAMPLES);

    // Channel layout is reported on the codec parameters after decoding the first packet.
    let params = dec.codec_params();
    assert_eq!(params.sample_rate, Some(48000));
    let channels = params.channels.clone().unwrap();
    assert_eq!(channels.count(), 6);
    assert_eq!(
        channels,
        Position::FRONT_LEFT
            .union(Position::FRONT_RIGHT)
            .union(Position::FRONT_CENTER)
            .union(Position::LFE1)
            .union(Position::REAR_LEFT)
            .union(Position::REAR_RIGHT)
            .into()
    );

    check(&decode_all(&packets));
}

#[test]
fn decodes_alternate_bitstream_forms() {
    let packets = core_packets();

    let le16: Vec<_> = packets.iter().map(|p| to_le16(p)).collect();
    check(&decode_all(&le16));

    let be14: Vec<_> = packets.iter().map(|p| to_14bit(p, false)).collect();
    check(&decode_all(&be14));

    let le14: Vec<_> = packets.iter().map(|p| to_14bit(p, true)).collect();
    check(&decode_all(&le14));
}

#[test]
fn corrupt_packets_do_not_panic() {
    let packets = core_packets();
    let mut dec = new_decoder();

    // Deterministic xorshift generator.
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for _ in 0..300 {
        let mut p = packets[(rnd() % 3) as usize].clone();

        match rnd() % 3 {
            0 => p.truncate((rnd() as usize) % p.len()),
            1 => {
                for _ in 0..1 + rnd() % 8 {
                    let i = (rnd() as usize) % p.len();
                    p[i] ^= 1 << (rnd() % 8);
                }
            }
            _ => {
                for _ in 0..1 + rnd() % 4 {
                    let i = (rnd() as usize) % p.len();
                    p[i] = rnd() as u8;
                }
            }
        }

        let _ = decode_packet(&mut dec, &p);
    }

    // The decoder must still decode valid packets after errors.
    dec.reset();
    assert!(decode_packet(&mut dec, &packets[0]).is_some());
}

#[test]
fn rejects_garbage() {
    let mut dec = new_decoder();
    assert!(decode_packet(&mut dec, &[0u8; 64]).is_none());
    assert!(decode_packet(&mut dec, &[0x7f, 0xfe, 0x80, 0x01]).is_none());
}
