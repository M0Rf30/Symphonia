// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decoder output of the WAVE and AIFF-C ADPCM variants of this crate, checked against vectors
//! produced by FFmpeg 9.0.2 (`ffmpeg -i case.wav -f s16le -`) for the same input bytes. Every
//! case lists the 16-bit samples that FFmpeg decoded, interleaved.

use symphonia_core::audio::{Audio, Channels, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::AdpcmDecoder;

struct Case {
    name: &'static str,
    codec: AudioCodecId,
    channels: u16,
    /// Frames of one block (block based codecs), or of the whole packet (all others).
    frames_per_block: u64,
    bits: Option<u32>,
    /// The packet payload as hex.
    data: &'static str,
    /// The interleaved 16-bit samples decoded by FFmpeg.
    expected: &'static [i16],
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn decode(case: &Case) -> Vec<i16> {
    let channels = usize::from(case.channels);
    let frames = (case.expected.len() / channels) as u64;
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(case.codec)
        .with_sample_rate(8000)
        .with_channels(Channels::Discrete(case.channels))
        .with_max_frames_per_packet(frames)
        .with_frames_per_block(case.frames_per_block);
    if let Some(bits) = case.bits {
        params.with_bits_per_coded_sample(bits);
    }
    let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let data = unhex(case.data);
    let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(frames), &data);
    let buf = decoder.decode_ref(&packet).unwrap();
    let mut out = vec![0i32; buf.frames() * channels];
    match buf {
        GenericAudioBufferRef::S32(b) => b.copy_to_slice_interleaved(&mut out),
        _ => unreachable!(),
    }
    out.iter().map(|&s| (s >> 16) as i16).collect()
}

#[test]
fn wave_and_aiff_variants_match_ffmpeg() {
    #[rustfmt::skip]
    let cases = [
    Case { name: "ms3", codec: CODEC_ID_ADPCM_MS, channels: 3, frames_per_block: 12, bits: None, data: "069603c9cfcab32e112a32b5013300c3b9eeb5b1f7ed4c2e01c5052acf9eb5f97f21ee23047605a4c8b935178a209af606e70216f6a8f166e8092402023601eb2a7912551b27fe53", expected: &[-19510, -18962, -19042, -12343, -17981, -12502, 617, -17255, -7439, 10482, -16448, -11658, 16231, -15713, 6383, 16018, -14530, 16797, 11010, -13653, 32767, -868, -13187, 32767, -8096, -12229, 21707, -9688, -11859, 711, -11812, -11137, -11359, -2417, -10731, -11402, 13753, -3672, 4729, -14172, -2538, 10987, -11888, 3900, 1550, -2353, 17187, 2475, -26301, 16839, 790, -32768, -11151, -3545, 5424, -32335, 2264, 5085, -32768, 7119, -32768, 13646, -2439, -32768, 32767, -4382, -32768, 32767, 9840, 32767, 32767, 9432] },
    Case { name: "dk4_1ch", codec: CODEC_ID_ADPCM_IMA_DK4, channels: 1, frames_per_block: 25, bits: None, data: "90a647006e490db138489ce814d58d14ccda22004f994fed15c5b2fdaeeff317", expected: &[-22896, -12360, -31026, -8133, -17366, -14568, -32768, -32768, -22611, -1066, -3864, 19029, 15952, 7558, -15335, -32768, -32768, -21596, 8875, -32768, 12286, 8191, -32768, -20481, 13037, -9524, -9311, -9742, -9927, -10095, -9636, -10562, -12284, -14866, -13836, -10400, -14517, -8429, -14102, -10419, -20465, -32768, -32768, -32768, -32768, -32768, -32768, -4097, 7075, 32767] },
    Case { name: "dk4_2ch", codec: CODEC_ID_ADPCM_IMA_DK4, channels: 2, frames_per_block: 17, bits: None, data: "11714c0006d73800e0978c3f5fd5df3d34f8c08262b03750e5091300a02539002428ca6d189213702ca29ceb218325da", expected: &[28945, -10490, 11977, -10296, 5040, -7651, 2938, -11053, 16315, -17915, 32767, -32624, 4786, -9500, -32768, -32768, -4097, -32768, 21972, 4094, -28814, -1, -32768, 3723, -32768, 20651, 15647, 32767, -13024, 32767, 13045, 32767, 32767, 32767, 2533, 9632, 2561, 11552, 2586, 11294, 2545, 10121, 2618, 7774, 2648, 7462, 2621, 8882, 2645, 10689, 2757, 10923, 2838, 9003, 2765, 10294, 2725, 8182, 2568, 6194, 2676, 6968, 2657, 8611, 2746, 10958, 2568, 9397] },
    Case { name: "dk3", codec: CODEC_ID_ADPCM_IMA_DK3, channels: 2, frames_per_block: 16, bits: None, data: "6733cb63eb78b869d759b61d28061a261eb44efff1aa474318544a23a657001f2c4b6f14ddc8e60f83f51a56c38f9bd8a34d2f858ed2cc8d", expected: &[9143, 5783, 9319, 5959, 8742, 6204, 8936, 6398, 7704, 6848, 7872, 7016, 6506, 6852, 5959, 6305, 8436, 6812, 9929, 8305, 11543, 10183, 12247, 10887, 15491, 11483, 14200, 10192, 17546, 11070, 18966, 12490, -28621, -28621, -28771, -28771, -28792, -28792, -28850, -28850, -28867, -28867, -28754, -28754, -28827, -28827, -28707, -28707, -8471, 16107, -8088, 16490, -28618, -28618, -28664, -28664, -28454, -28454, -28799, -28799, -29216, -29216, -29272, -29272] },
    Case { name: "xbox_1ch", codec: CODEC_ID_ADPCM_IMA_XBOX, channels: 1, frames_per_block: 16, bits: Some(4), data: "3bba30008c6d98cb1ab2e177227b1500c29d0125f5ca98db", expected: &[-17861, -18675, -18784, -19878, -17984, -18242, -18946, -20439, -22185, -23358, -22718, -21748, -22982, -22502, -24396, -20523, 31522, 31556, 31500, 31418, 31385, 31415, 31424, 31514, 31574, 31695, 31452, 31278, 30994, 30956, 30852, 30631] },
    Case { name: "xbox_2ch", codec: CODEC_ID_ADPCM_IMA_XBOX, channels: 2, frames_per_block: 16, bits: Some(4), data: "8c7517000f4d5000f45090bdb16956eaf20eef350dbbf321b0c74b00de292c009498a996638e2568adaba4ea882b3d7d", expected: &[30092, 19727, 30166, 25460, 30016, 13299, 30037, 8561, 30252, 27227, 30280, 32767, 30202, 32767, 29941, 12288, 29697, -32768, 29855, -32768, 29424, -28673, 28622, -32768, 28731, -32768, 27239, -11223, 24466, -32768, 28624, -20481, -14416, 10718, -3737, 11150, -8044, 11879, -9349, 10586, -12908, 10410, -16144, 12172, -21047, 13345, -9458, 13132, -14196, 15654, -29990, 15311, -32768, 14999, -32768, 13011, -32768, 14302, -18554, 11720, -28109, 14125, -32768, 10689] },
    Case { name: "ct_1ch", codec: CODEC_ID_ADPCM_CT, channels: 1, frames_per_block: 48, bits: None, data: "83be460eca13166a4fa0b5de239c85f870b22a09a97553f4", expected: &[-63, 384, -66, -896, 260, 2247, 2535, -1061, -6005, -9257, -7408, -3625, -2163, 3438, 14581, 5875, 19723, -8202, -28617, -24715, -32768, 160, -32768, -32768, -12033, 13820, 3794, -22968, -26350, 9055, -32768, -32768, 22686, 26603, 636, 17161, 31877, 18285, 20539, 13917, 4133, -1115, 22319, 32767, 32767, 32767, -22687, 14352] },
    Case { name: "oki_1ch", codec: CODEC_ID_ADPCM_IMA_OKI, channels: 1, frames_per_block: 48, bits: None, data: "83be460eca13166a4fa0b5de239c85f870b22a09a97553f4", expected: &[-32, 192, -32, -448, 48, 928, 1040, -384, -2128, -3296, -2656, -1312, -784, 1296, 5008, 2480, 6608, -1760, -7728, -6640, -13552, -3680, -18144, -32768, -17248, 2496, -5184, -26160, -28976, -784, -32768, -32768, 9552, 12656, -7088, 5728, 17376, 6784, 8704, 3456, -4496, -8832, 10896, 32752, 32752, 32752, -9568, 18368] },
    Case { name: "ws_1ch", codec: CODEC_ID_ADPCM_IMA_WS, channels: 1, frames_per_block: 48, bits: None, data: "83be460eca13166a4fa0b5de239c85f870b22a09a97553f4", expected: &[6, 6, -5, -16, 3, 26, -14, -9, -34, -75, -36, -21, 39, 63, 26, 115, -66, 169, 200, 57, 344, 76, -377, -1056, -423, -12, -684, -955, -51, -171, -280, -1772, -1559, 1351, 3430, 784, -934, 627, -225, 33, -671, -1737, 397, 4657, 8919, 15007, 22301, 7592] },
    Case { name: "ct_2ch", codec: CODEC_ID_ADPCM_CT, channels: 2, frames_per_block: 24, bits: None, data: "ff47224a7c54c9a742e414be23bcd11624a07465b1c2fc1a", expected: &[-958, -958, 427, 1345, 1341, 3170, 2813, 1496, 5755, -1182, 10922, 2024, 4023, 730, -547, 6463, 6796, 11000, -5970, 18333, -57, 27085, -12356, 11464, -4368, 27969, -14260, 8582, -28163, 16176, -21837, 32767, -12524, 32767, -20641, 32767, 1660, 32767, 32767, 32767, 3840, 32767, -29308, 32767, -32768, 2756, -20225, -17089] },
    Case { name: "oki_2ch", codec: CODEC_ID_ADPCM_IMA_OKI, channels: 2, frames_per_block: 24, bits: None, data: "ff47224a7c54c9a742e414be23bcd11624a07465b1c2fc1a", expected: &[-480, -480, 128, 528, 528, 1248, 1184, 592, 2528, -480, 4656, 832, 2096, 304, 368, 2704, 3184, 4432, -1744, 7248, 272, 10656, -4016, 4688, -1232, 10400, -4768, 3728, -9824, 6416, -7808, 17024, -4752, 30048, -7536, 31792, 48, 32752, 14192, 32752, 720, 32752, -15040, 32752, -32768, 11776, -23456, -2320] },
    Case { name: "ws_2ch", codec: CODEC_ID_ADPCM_IMA_WS, channels: 2, frames_per_block: 24, bits: None, data: "ff47224a7c54c9a742e414be23bcd11624a07465b1c2fc1a", expected: &[-13, 13, -43, 31, -22, 20, -3, 39, -34, 62, 29, 96, 2, 165, -72, 115, -22, 197, 60, 54, 159, -201, 199, -445, 283, -729, 338, -997, 368, -544, 268, -359, 388, -303, 469, -558, 601, -48, 869, 836, 984, 1437, 740, 452, 456, -210, -119, 151] },
    ];

    for case in &cases {
        assert_eq!(decode(case), case.expected, "{}", case.name);
    }
}

/// FFmpeg decodes G.722, G.726 and the headerless 4-bit codecs for at most 1 (or 2) channels, so
/// the multichannel layout (the codes of the channels alternate) is checked against the
/// FFmpeg-verified mono decoders: channel `c` of an interleaved stream must decode exactly like
/// the mono stream of the codes `c`, `c + n`, `c + 2n`, ...
mod interleaved {
    use super::*;

    /// A deterministic code generator (an LCG).
    fn codes(count: usize, bits: u32, seed: u32) -> Vec<u32> {
        let mut x = seed;
        (0..count)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 16) & ((1 << bits) - 1)
            })
            .collect()
    }

    /// Packs `codes` of `bits` bits: MSB first (`msb`) or little-endian bit order.
    fn pack(codes: &[u32], bits: u32, msb: bool) -> Vec<u8> {
        let mut out = vec![0u8; (codes.len() * bits as usize).div_ceil(8)];
        for (i, &code) in codes.iter().enumerate() {
            for b in 0..bits {
                let bit = if msb { (code >> (bits - 1 - b)) & 1 } else { (code >> b) & 1 };
                let pos = i * bits as usize + b as usize;
                let at = if msb { 7 - pos % 8 } else { pos % 8 };
                out[pos / 8] |= (bit as u8) << at;
            }
        }
        out
    }

    fn run(codec: AudioCodecId, channels: u16, bits: Option<u32>, data: &[u8]) -> Vec<Vec<i16>> {
        let frames = match codec {
            CODEC_ID_ADPCM_G722 => 2 * data.len() / usize::from(channels),
            CODEC_ID_ADPCM_G726 | CODEC_ID_ADPCM_G726LE => {
                data.len() * 8 / (bits.unwrap() as usize * usize::from(channels))
            }
            _ => 2 * data.len() / usize::from(channels),
        };
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(codec)
            .with_sample_rate(8000)
            .with_channels(Channels::Discrete(channels))
            .with_max_frames_per_packet(frames as u64);
        if let Some(bits) = bits {
            params.with_bits_per_coded_sample(bits);
        }
        let mut decoder = AdpcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(frames as u64), data);
        let buf = decoder.decode_ref(&packet).unwrap();
        let GenericAudioBufferRef::S32(buf) = buf
        else {
            unreachable!()
        };
        assert_eq!(buf.frames(), frames);
        (0..usize::from(channels))
            .map(|c| buf.plane(c).unwrap().iter().map(|&s| (s >> 16) as i16).collect())
            .collect()
    }

    #[test]
    fn g722_channels_decode_like_mono() {
        for channels in [2usize, 3, 6] {
            let bytes: Vec<Vec<u32>> = (0..channels).map(|c| codes(40, 8, 7 + c as u32)).collect();
            let stream: Vec<u8> =
                (0..40).flat_map(|i| bytes.iter().map(move |ch| ch[i] as u8)).collect();
            let out = run(CODEC_ID_ADPCM_G722, channels as u16, None, &stream);
            for (c, out) in out.iter().enumerate() {
                let mono: Vec<u8> = bytes[c].iter().map(|&b| b as u8).collect();
                assert_eq!(out, &run(CODEC_ID_ADPCM_G722, 1, None, &mono)[0], "{channels}ch {c}");
            }
        }
    }

    #[test]
    fn g726_channels_decode_like_mono() {
        for (codec, msb) in [(CODEC_ID_ADPCM_G726, true), (CODEC_ID_ADPCM_G726LE, false)] {
            for bits in 2..=5u32 {
                for channels in [2usize, 3] {
                    // 40 frames: a whole number of bytes for every width.
                    let per: Vec<Vec<u32>> =
                        (0..channels).map(|c| codes(40, bits, 11 + c as u32)).collect();
                    let all: Vec<u32> =
                        (0..40).flat_map(|i| per.iter().map(move |ch| ch[i])).collect();
                    let out = run(codec, channels as u16, Some(bits), &pack(&all, bits, msb));
                    for (c, out) in out.iter().enumerate() {
                        let mono = run(codec, 1, Some(bits), &pack(&per[c], bits, msb));
                        assert_eq!(out, &mono[0], "{codec:?} {bits} bits {channels}ch {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn nibble_codecs_channels_decode_like_mono() {
        for codec in [CODEC_ID_ADPCM_YAMAHA, CODEC_ID_ADPCM_CT, CODEC_ID_ADPCM_IMA_OKI] {
            // The first code of a byte is the low nibble for Yamaha and the high one otherwise.
            let low_first = codec == CODEC_ID_ADPCM_YAMAHA;
            let nibbles = |codes: &[u32]| -> Vec<u8> {
                codes
                    .chunks(2)
                    .map(|p| {
                        let (a, b) = (p[0] as u8, p[1] as u8);
                        if low_first { a | (b << 4) } else { (a << 4) | b }
                    })
                    .collect()
            };
            for channels in [2usize, 3, 4, 5, 8] {
                let per: Vec<Vec<u32>> =
                    (0..channels).map(|c| codes(40, 4, 3 + c as u32)).collect();
                let all: Vec<u32> = (0..40).flat_map(|i| per.iter().map(move |ch| ch[i])).collect();
                let out = run(codec, channels as u16, None, &nibbles(&all));
                for (c, out) in out.iter().enumerate() {
                    let mono = run(codec, 1, None, &nibbles(&per[c]));
                    assert_eq!(out, &mono[0], "{codec:?} {channels}ch {c}");
                }
            }
        }
    }
}
