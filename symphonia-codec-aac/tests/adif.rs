//! ADIF: header parsing, block splitting by parsing the block syntax, duration estimate,
//! decoding, and seeking.

use symphonia_codec_aac::{AacDecoder, AdifReader, AdtsReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo, Track};
use symphonia_core::io::{MediaSourceStream, ReadOnlySource};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

const AAC_LC_22K_MONO: &[u8] = include_bytes!("fixtures/aac_lc_22k_mono.aac");
const HE_AAC_V1_STEREO: &[u8] = include_bytes!("fixtures/he_aac_v1_stereo.aac");
const HE_AAC_V1_MONO: &[u8] = include_bytes!("fixtures/he_aac_v1_mono.aac");
const HE_AAC_V2: &[u8] = include_bytes!("fixtures/he_aac_v2.aac");
const MULTICHANNEL_5P1: &[u8] = include_bytes!("fixtures/multichannel_5p1.aac");

/// A MSB-first bit writer.
#[derive(Default)]
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

    fn align(&mut self) {
        self.n_bits = self.n_bits.next_multiple_of(8);
    }
}

/// A program config element: (profile, sampling frequency index, front, side, back, lfe), where
/// the front, side, and back elements are true for a channel pair.
struct Program {
    profile: u32,
    sf_index: u32,
    front: Vec<bool>,
    side: Vec<bool>,
    back: Vec<bool>,
    lfe: u32,
}

impl Program {
    fn write(&self, bw: &mut BitWriter, comment: &[u8]) {
        bw.put(0, 4); // element_instance_tag
        bw.put(self.profile, 2);
        bw.put(self.sf_index, 4);
        bw.put(self.front.len() as u32, 4);
        bw.put(self.side.len() as u32, 4);
        bw.put(self.back.len() as u32, 4);
        bw.put(self.lfe, 2);
        bw.put(0, 3); // num_assoc_data_elements
        bw.put(0, 4); // num_valid_cc_elements
        bw.put(0, 3); // No mixdowns.

        for elements in [&self.front, &self.side, &self.back] {
            for (tag, &is_cpe) in elements.iter().enumerate() {
                bw.put(u32::from(is_cpe), 1);
                bw.put(tag as u32, 4);
            }
        }

        for tag in 0..self.lfe {
            bw.put(tag, 4);
        }

        bw.align();
        bw.put(comment.len() as u32, 8);

        for &b in comment {
            bw.put(u32::from(b), 8);
        }
    }
}

struct HeaderOptions {
    copyright: bool,
    variable_bitrate: bool,
    comment: &'static [u8],
}

impl Default for HeaderOptions {
    fn default() -> Self {
        HeaderOptions { copyright: false, variable_bitrate: false, comment: b"" }
    }
}

/// Builds an `adif_header()` with the programs.
fn adif_header(programs: &[Program], opts: &HeaderOptions) -> Vec<u8> {
    let mut bw = BitWriter::default();

    for &b in b"ADIF" {
        bw.put(u32::from(b), 8);
    }

    bw.put(opts.copyright as u32, 1);

    if opts.copyright {
        for _ in 0..9 {
            bw.put(0x43, 8);
        }
    }

    bw.put(1, 1); // original_copy
    bw.put(0, 1); // home
    bw.put(opts.variable_bitrate as u32, 1);
    bw.put(128_000, 23);
    bw.put(programs.len() as u32 - 1, 4);

    for program in programs {
        if !opts.variable_bitrate {
            bw.put(0x12345, 20); // adif_buffer_fullness
        }

        program.write(&mut bw, opts.comment);
    }

    bw.bytes
}

fn mono_program(sf_index: u32) -> Program {
    Program { profile: 1, sf_index, front: vec![false], side: vec![], back: vec![], lfe: 0 }
}

fn stereo_program(sf_index: u32) -> Program {
    Program { profile: 1, sf_index, front: vec![true], side: vec![], back: vec![], lfe: 0 }
}

fn program_5p1(sf_index: u32) -> Program {
    Program {
        profile: 1,
        sf_index,
        front: vec![false, true],
        side: vec![],
        back: vec![true],
        lfe: 1,
    }
}

fn mss_of(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default())
}

/// The `raw_data_block()`s of an ADTS stream.
fn blocks_of(adts: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = AdtsReader::try_probe_new(mss_of(adts.to_vec()), Default::default()).unwrap();
    let mut blocks = vec![];

    while let Some(packet) = reader.next_packet().unwrap() {
        blocks.push(packet.data.to_vec());
    }

    blocks
}

fn track_params(reader: &dyn FormatReader) -> (Track, AudioCodecParameters) {
    let track = reader.tracks()[0].clone();
    match track.codec_params.clone() {
        Some(CodecParameters::Audio(params)) => (track, params),
        _ => panic!("expected audio codec parameters"),
    }
}

fn adif_of(program: Program, opts: &HeaderOptions, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut data = adif_header(&[program], opts);
    data.extend(blocks.iter().flatten());
    data
}

fn decode(params: &AudioCodecParameters, blocks: &[Vec<u8>]) -> Vec<f32> {
    let mut decoder = AacDecoder::try_new(params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];

    for (i, block) in blocks.iter().enumerate() {
        let packet = Packet::new(
            0,
            Timestamp::new(i as i64 * 1024),
            Duration::new(1024),
            block.clone().into_boxed_slice(),
        );

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        for c in 0..buf.spec().channels().count() {
            samples.extend_from_slice(buf.plane(c).unwrap());
        }
    }

    samples
}

#[test]
fn splits_blocks_of_mono_stereo_and_multichannel_streams() {
    // (ADTS stream, ADIF program, channels, rate, sampling frequency index)
    for (name, adts, program, channels, rate) in [
        ("mono 22.05k", AAC_LC_22K_MONO, mono_program(7), 1, 22_050),
        ("5.1 48k", MULTICHANNEL_5P1, program_5p1(3), 6, 48_000),
    ] {
        let blocks = blocks_of(adts);
        assert!(blocks.len() > 10, "{name}");

        let data = adif_of(program, &HeaderOptions::default(), &blocks);
        let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        assert_eq!(params.sample_rate, Some(rate), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(rate), "{name}");

        // The boundaries of the blocks are those of the ADTS frames.
        let mut found = vec![];
        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(packet.pts.get(), found.len() as i64 * 1024, "{name}");
            assert_eq!(packet.dur.get(), 1024, "{name}");
            found.push(packet.data.to_vec());
        }

        assert_eq!(found, blocks, "{name}");

        // And the audio decoded from the parameters is that of the ADTS stream.
        let adts_reader =
            AdtsReader::try_probe_new(mss_of(adts.to_vec()), Default::default()).unwrap();
        let (_, adts_params) = track_params(adts_reader.as_ref());

        let samples = decode(&params, &found);
        assert_eq!(samples, decode(&adts_params, &blocks), "{name}");
        assert!(samples.iter().any(|s| s.abs() > 1e-4), "{name}: silence");
    }
}

#[test]
fn header_variants() {
    let blocks = blocks_of(AAC_LC_22K_MONO);

    for (name, opts) in [
        ("cbr", HeaderOptions::default()),
        ("vbr", HeaderOptions { variable_bitrate: true, ..Default::default() }),
        ("copyright id", HeaderOptions { copyright: true, ..Default::default() }),
        ("comment", HeaderOptions { comment: b"a comment in the header", ..Default::default() }),
        (
            "everything",
            HeaderOptions { copyright: true, variable_bitrate: true, comment: &[0x55; 255] },
        ),
    ] {
        let data = adif_of(mono_program(7), &opts, &blocks);
        let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();

        let mut found = vec![];
        while let Some(packet) = reader.next_packet().unwrap() {
            found.push(packet.data.to_vec());
        }

        assert_eq!(found, blocks, "{name}");
    }
}

#[test]
fn reports_implicit_sbr_and_ps_like_adts() {
    for (name, adts, program, channels) in [
        ("he-aac v1 stereo", HE_AAC_V1_STEREO, stereo_program(7), 2),
        ("he-aac v1 mono", HE_AAC_V1_MONO, mono_program(7), 1),
        ("he-aac v2", HE_AAC_V2, mono_program(7), 2),
    ] {
        let blocks = blocks_of(adts);
        let data = adif_of(program, &HeaderOptions::default(), &blocks);
        let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(44_100), "{name}");

        let mut found = vec![];
        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(packet.pts.get(), found.len() as i64 * 2048, "{name}");
            assert_eq!(packet.dur.get(), 2048, "{name}");
            found.push(packet.data.to_vec());
        }

        assert_eq!(found, blocks, "{name}");

        // The same audio as the ADTS stream.
        let adts_reader =
            AdtsReader::try_probe_new(mss_of(adts.to_vec()), Default::default()).unwrap();
        let (_, adts_params) = track_params(adts_reader.as_ref());
        assert_eq!(decode(&params, &found), decode(&adts_params, &blocks), "{name}");
    }
}

#[test]
fn estimates_the_duration() {
    // The real duration of the stream is within the estimate's tolerance. The fixtures are short,
    // so the blocks examined are the whole stream.
    let blocks = blocks_of(AAC_LC_22K_MONO);
    let data = adif_of(mono_program(7), &HeaderOptions::default(), &blocks);

    let reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
    let (track, _) = track_params(reader.as_ref());

    assert_eq!(track.num_frames, Some(blocks.len() as u64 * 1024));
    assert_eq!(track.duration.map(|d| d.get()), Some(blocks.len() as u64 * 1024));
}

#[test]
fn rejects_unsupported_streams() {
    let blocks = blocks_of(AAC_LC_22K_MONO);

    // Not ADIF.
    assert!(AdifReader::try_probe_new(mss_of(blocks.concat()), Default::default()).is_err());

    // Several programs.
    let mut data = adif_header(&[mono_program(7), mono_program(7)], &HeaderOptions::default());
    data.extend(blocks.iter().flatten());
    assert!(matches!(
        AdifReader::try_probe_new(mss_of(data), Default::default()),
        Err(Error::Unsupported(_))
    ));

    // A header without blocks.
    let data = adif_header(&[mono_program(7)], &HeaderOptions::default());
    assert!(AdifReader::try_probe_new(mss_of(data), Default::default()).is_err());

    // A truncated header.
    let mut data = adif_header(&[mono_program(7)], &HeaderOptions::default());
    data.truncate(10);
    assert!(AdifReader::try_probe_new(mss_of(data), Default::default()).is_err());

    // A reserved sampling frequency index.
    let data = adif_of(mono_program(13), &HeaderOptions::default(), &blocks);
    assert!(AdifReader::try_probe_new(mss_of(data), Default::default()).is_err());
}

#[test]
fn trailing_data_and_truncation_end_the_stream() {
    let blocks = blocks_of(AAC_LC_22K_MONO);

    // Padding and a tag after the last block, and a block cut short.
    for (name, tail) in [
        ("zeros", vec![0u8; 64]),
        ("tag", [&b"TAG"[..], &[b' '; 125]].concat()),
        ("cut block", blocks[3][..blocks[3].len() / 2].to_vec()),
    ] {
        let mut data = adif_of(mono_program(7), &HeaderOptions::default(), &blocks);
        data.extend(&tail);

        let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let mut n = 0;

        while reader.next_packet().unwrap().is_some() {
            n += 1;
        }

        assert!(n >= blocks.len(), "{name}: lost blocks");
        assert!(n <= blocks.len() + 1, "{name}: invented blocks");
    }
}

#[test]
fn reads_from_a_stream_that_cannot_be_rewound() {
    let blocks = blocks_of(AAC_LC_22K_MONO);
    let data = adif_of(mono_program(7), &HeaderOptions::default(), &blocks);

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(std::io::Cursor::new(data))),
        Default::default(),
    );

    let mut reader = AdifReader::try_probe_new(mss, Default::default()).unwrap();

    let mut found = vec![];
    while let Some(packet) = reader.next_packet().unwrap() {
        found.push(packet.data.to_vec());
    }

    assert_eq!(found, blocks);
}

/// A long stream of AAC-LC blocks at 48 kHz (which cannot have implicit SBR): the blocks of the
/// fixture, repeated.
fn long_stream() -> (Vec<u8>, Vec<Vec<u8>>) {
    let one = blocks_of(MULTICHANNEL_5P1);
    let blocks: Vec<Vec<u8>> = (0..5).flat_map(|_| one.clone()).collect();
    assert!(blocks.len() > 100);
    (adif_of(program_5p1(3), &HeaderOptions::default(), &blocks), blocks)
}

fn seek_to(reader: &mut dyn FormatReader, block: u64, offset: i64) -> (i64, i64) {
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(block as i64 * 1024 + offset), track_id: 0 },
        )
        .unwrap();

    (seeked.required_ts.get(), seeked.actual_ts.get())
}

#[test]
fn seeks_by_block_with_preroll() {
    let (data, blocks) = long_stream();
    let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();

    // Forward, into a block that was never parsed. A decoder is fed the block before the target.
    let (required, actual) = seek_to(reader.as_mut(), 70, 100);
    assert_eq!(required, 70 * 1024 + 100);
    assert_eq!(actual, 69 * 1024);

    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts.get(), 69 * 1024);
    assert_eq!(&packet.data[..], &blocks[69][..]);

    // Backward, to a block with a remembered position and to one without (in between).
    for block in [5, 33, 34, 64, 65, 66, 3, 1, 0, 90, 17] {
        let (_, actual) = seek_to(reader.as_mut(), block, 0);
        let start = block.saturating_sub(1);
        assert_eq!(actual, start as i64 * 1024, "block {block}");

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts.get(), start as i64 * 1024, "block {block}");
        assert_eq!(&packet.data[..], &blocks[start as usize][..], "block {block}");

        // Reading continues after the seek.
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(&packet.data[..], &blocks[start as usize + 1][..], "block {block}");
    }

    // Past the end of the stream.
    let err = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(blocks.len() as i64 * 1024 * 4), track_id: 0 },
        )
        .unwrap_err();
    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn seeks_with_sbr_start_at_a_multiple_of_16_blocks() {
    let one = blocks_of(HE_AAC_V1_STEREO);
    let blocks: Vec<Vec<u8>> = (0..4).flat_map(|_| one.clone()).collect();
    let data = adif_of(stereo_program(7), &HeaderOptions::default(), &blocks);
    let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();

    // The packets are 2048 frames.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(90 * 2048 + 1), track_id: 0 },
        )
        .unwrap();

    // 90 - 41 = 49 -> 48.
    assert_eq!(seeked.actual_ts.get(), 48 * 2048);
    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 48 * 2048);
}

#[test]
fn seeks_forward_only_in_a_stream_that_cannot_be_rewound() {
    let (data, blocks) = long_stream();

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(std::io::Cursor::new(data))),
        Default::default(),
    );
    let mut reader = AdifReader::try_probe_new(mss, Default::default()).unwrap();

    let (_, actual) = seek_to(reader.as_mut(), 40, 0);
    assert_eq!(actual, 39 * 1024);
    assert_eq!(&reader.next_packet().unwrap().unwrap().data[..], &blocks[39][..]);

    // The stream is at block 40: seeking before it fails, seeking to it starts at the next block.
    let err = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(10 * 1024), track_id: 0 })
        .unwrap_err();
    assert!(matches!(err, Error::SeekError(SeekErrorKind::ForwardOnly)));
}

/// The generated format-coverage samples are used if `RMPD_SAMPLES` points at them. FDK's ADIF
/// output has no `adif_header()` (just the blocks, byte aligned), so a header is put in front of
/// it.
#[test]
fn sample_blocks_with_a_header() {
    let Some(dir) = std::env::var_os("RMPD_SAMPLES")
    else {
        return;
    };

    let Ok(blocks) = std::fs::read(std::path::Path::new(&dir).join("aac/aac_lc_fdk_adif.aac"))
    else {
        return;
    };

    let mut data = adif_header(&[stereo_program(4)], &HeaderOptions::default());
    data.extend(&blocks);

    let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
    let (track, params) = track_params(reader.as_ref());

    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));

    // The estimate is within 5% of the real duration.
    let mut n = 0u64;
    let mut found_bytes = 0;
    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut frames = 0;

    while let Some(packet) = reader.next_packet().unwrap() {
        found_bytes += packet.data.len();
        frames += decoder.decode(&packet).unwrap().frames();
        n += 1;
    }

    // Every byte of the sample is a block, and every block decodes.
    assert_eq!(found_bytes, blocks.len());
    assert_eq!(frames as u64, n * 1024);

    let estimate = track.duration.unwrap().get();
    assert!(estimate.abs_diff(n * 1024) * 20 < n * 1024, "{estimate} vs {}", n * 1024);
}

/// An AAC-LC `raw_data_block()` of a stereo CPE with every band zero, optionally preceded by an
/// in-band `program_config_element()`.
fn silent_stereo_block(with_pce: bool) -> Vec<u8> {
    let mut bw = BitWriter::default();

    if with_pce {
        bw.put(5, 3); // ID_PCE
        stereo_program(4).write(&mut bw, b"in-band");
    }

    bw.put(1, 3); // ID_CPE
    bw.put(0, 4); // element_instance_tag
    bw.put(1, 1); // common_window
    // ics_info(): reserved, ONLY_LONG_SEQUENCE, window shape, max_sfb = 0, no predictor.
    bw.put(0, 1);
    bw.put(0, 2);
    bw.put(0, 1);
    bw.put(0, 6);
    bw.put(0, 1);
    bw.put(0, 2); // ms_mask_present

    for _ in 0..2 {
        bw.put(100, 8); // global_gain
        bw.put(0, 3); // pulse_data_present, tns_data_present, gain_control_data_present
    }

    bw.put(7, 3); // ID_END
    bw.align();
    bw.bytes
}

#[test]
fn skips_a_program_config_element_in_a_block() {
    let blocks = vec![
        silent_stereo_block(false),
        silent_stereo_block(true),
        silent_stereo_block(false),
        silent_stereo_block(true),
    ];

    let data = adif_of(stereo_program(4), &HeaderOptions::default(), &blocks);
    let mut reader = AdifReader::try_probe_new(mss_of(data), Default::default()).unwrap();
    let (_, params) = track_params(reader.as_ref());

    let mut found = vec![];
    while let Some(packet) = reader.next_packet().unwrap() {
        found.push(packet.data.to_vec());
    }

    assert_eq!(found, blocks);

    // The blocks decode (to silence).
    let samples = decode(&params, &found);
    assert_eq!(samples.len(), 4 * 1024 * 2);
    assert!(samples.iter().all(|&s| s == 0.0));
}

/// Deterministic mutation fuzz test: corrupting an ADIF stream (header or blocks) must never
/// panic or hang the reader or the decoder, whatever the result.
#[test]
fn mutation_fuzz_never_panics() {
    let blocks = blocks_of(MULTICHANNEL_5P1);
    let data = adif_of(program_5p1(3), &HeaderOptions::default(), &blocks);

    let mut state = 0x2545f491u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };

    for trial in 0..400 {
        let mut mutated = data.clone();

        for _ in 0..1 + trial % 4 {
            // Half of the mutations hit the header.
            let range = if trial % 2 == 0 { 40 } else { mutated.len() };
            let pos = next() as usize % range;
            mutated[pos] ^= 1 << (next() % 8);
        }

        if trial % 7 == 0 {
            mutated.truncate(next() as usize % mutated.len());
        }

        let Ok(mut reader) = AdifReader::try_probe_new(mss_of(mutated), Default::default())
        else {
            continue;
        };

        let (_, params) = track_params(reader.as_ref());

        let Ok(mut decoder) = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
        else {
            continue;
        };

        let mut n = 0;

        while let Ok(Some(packet)) = reader.next_packet() {
            let _ = decoder.decode(&packet);
            n += 1;
            assert!(n <= blocks.len() * 2 + 4, "trial {trial}: too many packets");
        }
    }
}
