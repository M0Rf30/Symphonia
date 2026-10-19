#![allow(clippy::unwrap_used)]

//! LOAS packet durations of USAC streams: they are output frames, including the eSBR ratio.

use symphonia_codec_aac::{AacDecoder, LoasReader};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioDecoderOptions;
use symphonia_core::codecs::registry::RegisterableAudioDecoder;
use symphonia_core::formats::FormatReader;
use symphonia_core::io::MediaSourceStream;

struct BitWriter {
    bytes: Vec<u8>,
    n_bits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.n_bits % 8 == 0 {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= (((value >> i) & 1) as u8) << (7 - self.n_bits % 8);
            self.n_bits += 1;
        }
    }
}

/// An audio mux element (audioMuxVersion 1) carrying one access unit, with the stream mux config
/// (the fixture's `UsacConfig()` audio specific config) if `asc` is given.
fn mux_element(asc: Option<&[u8]>, au: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter { bytes: vec![], n_bits: 0 };
    bw.put(asc.is_none() as u32, 1); // useSameStreamMux

    if let Some(asc) = asc {
        bw.put(1, 1); // audioMuxVersion
        bw.put(0, 1); // audioMuxVersionA
        bw.put(0, 2); // taraBufferFullness: LatmGetValue, 1 byte
        bw.put(0, 8);
        bw.put(1, 1); // allStreamsSameTimeFraming
        bw.put(0, 6); // numSubFrames
        bw.put(0, 4); // numProgram - 1
        bw.put(0, 3); // numLayer - 1
        bw.put(1, 2); // ascLen: 2 bytes
        bw.put((asc.len() * 8) as u32, 16);
        for &b in asc {
            bw.put(u32::from(b), 8);
        }
        bw.put(0, 3); // frameLengthType
        bw.put(0xff, 8); // latmBufferFullness
        bw.put(0, 1); // otherDataPresent
        bw.put(0, 1); // crcCheckPresent
    }

    let mut len = au.len();
    while len >= 255 {
        bw.put(255, 8);
        len -= 255;
    }
    bw.put(len as u32, 8);
    for &b in au {
        bw.put(u32::from(b), 8);
    }
    bw.bytes
}

fn check(bin: &[u8], out_frame: usize) {
    let mut records = vec![];
    let mut pos = 0;
    while pos < bin.len() {
        let len = usize::from(u16::from_be_bytes([bin[pos], bin[pos + 1]]));
        records.push(&bin[pos + 2..pos + 2 + len]);
        pos += 2 + len;
    }
    let asc = records.remove(0);

    let mut data = vec![];
    for (i, au) in records.iter().enumerate() {
        let el = mux_element((i == 0).then_some(asc), au);
        data.extend([0x56, 0xe0 | (el.len() >> 8) as u8, el.len() as u8]);
        data.extend(el);
    }

    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut reader = LoasReader::try_new(mss, Default::default()).unwrap();

    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("expected audio parameters");
    };
    let mut decoder =
        AacDecoder::try_registry_new(&params, &AudioDecoderOptions::default()).unwrap();

    let (mut dur_sum, mut frames_sum, mut n) = (0, 0, 0);
    while let Some(packet) = reader.next_packet().unwrap() {
        assert_eq!(packet.pts.get() as u64, dur_sum);
        dur_sum += packet.dur.get();
        frames_sum += decoder.decode(&packet).unwrap().frames() as u64;
        n += 1;
    }

    assert_eq!(n, records.len());
    assert_eq!(dur_sum, frames_sum);
    assert_eq!(dur_sum, (records.len() * out_frame) as u64);
}

#[test]
fn usac_loas_durations_without_sbr() {
    check(include_bytes!("fixtures/usac_fd_mono.bin"), 1024);
}

#[test]
fn usac_loas_durations_sbr_2_1() {
    check(include_bytes!("fixtures/usac_sbr_mono.bin"), 2048);
}

#[test]
fn usac_loas_durations_sbr_8_3() {
    check(include_bytes!("fixtures/usac_sbr83.bin"), 2048);
}

#[test]
fn usac_loas_durations_sbr_4_1() {
    check(include_bytes!("fixtures/usac_sbr41.bin"), 4096);
}
