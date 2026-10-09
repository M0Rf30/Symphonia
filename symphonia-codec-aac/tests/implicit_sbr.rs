//! Implicitly signalled SBR and parametric stereo: HE-AAC in transports that do not signal it
//! (ADTS, and LOAS/LATM with a plain AAC-LC config).

use symphonia_codec_aac::{AacDecoder, AdtsReader, LoasReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef, layouts};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo, Track};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

const HE_AAC_V1_STEREO: &[u8] = include_bytes!("fixtures/he_aac_v1_stereo.aac");
const HE_AAC_V1_MONO: &[u8] = include_bytes!("fixtures/he_aac_v1_mono.aac");
const HE_AAC_V2: &[u8] = include_bytes!("fixtures/he_aac_v2.aac");
const AAC_LC_22K_MONO: &[u8] = include_bytes!("fixtures/aac_lc_22k_mono.aac");

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

    /// Pad with zero bits to the next byte boundary.
    fn align(&mut self) {
        while self.n_bits % 8 != 0 {
            self.put(0, 1);
        }
    }
}

fn mss_of(data: &[u8]) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default())
}

fn track_params(reader: &dyn FormatReader) -> (Track, AudioCodecParameters) {
    let track = reader.tracks()[0].clone();
    match track.codec_params.clone() {
        Some(CodecParameters::Audio(params)) => (track, params),
        _ => panic!("expected audio codec parameters"),
    }
}

type RawPacket = (i64, u64, Vec<u8>);

/// Reads every packet of a reader as (pts, duration, data).
fn read_packets(reader: &mut dyn FormatReader) -> Vec<RawPacket> {
    let mut packets = vec![];
    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push((packet.pts.get(), packet.dur.get(), packet.data.to_vec()));
    }
    packets
}

/// Decodes the packets, returning (rate, channels, frames) of every decoded buffer, and the
/// samples.
fn decode_packets(
    decoder: &mut AacDecoder,
    packets: &[RawPacket],
) -> (Vec<(u32, usize, usize)>, Vec<f32>) {
    let mut formats = vec![];
    let mut samples = vec![];

    for (pts, dur, data) in packets {
        let packet = Packet::new(
            0,
            Timestamp::new(*pts),
            Duration::new(*dur),
            data.clone().into_boxed_slice(),
        );

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        let n_ch = buf.spec().channels().count();
        formats.push((buf.spec().rate(), n_ch, buf.frames()));

        for c in 0..n_ch {
            samples.extend_from_slice(buf.plane(c).unwrap());
        }
    }

    (formats, samples)
}

#[test]
fn adts_reports_implicit_sbr_and_ps_in_params_and_timeline() {
    for (name, data, channels) in [
        ("he-aac v1 stereo", HE_AAC_V1_STEREO, 2),
        ("he-aac v1 mono", HE_AAC_V1_MONO, 1),
        ("he-aac v2", HE_AAC_V2, 2),
    ] {
        let mut reader = AdtsReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        // The parameters and the timeline describe the decoded output.
        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(44_100), "{name}");

        let packets = read_packets(reader.as_mut());
        assert_eq!(packets.len(), 24, "{name}");

        for (i, (pts, dur, _)) in packets.iter().enumerate() {
            assert_eq!(*dur, 2048, "{name}");
            assert_eq!(*pts, i as i64 * 2048, "{name}");
        }

        // The decoder created from the parameters outputs exactly what the parameters say.
        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        assert_eq!(decoder.codec_params().sample_rate, Some(44_100), "{name}");

        let (formats, _) = decode_packets(&mut decoder, &packets);

        for format in formats {
            assert_eq!(format, (44_100, channels, 2048), "{name}");
        }
    }
}

#[test]
fn adts_without_sbr_keeps_the_core_rate_and_timeline() {
    let mut reader =
        AdtsReader::try_probe_new(mss_of(AAC_LC_22K_MONO), Default::default()).unwrap();
    let (track, params) = track_params(reader.as_ref());

    assert_eq!(params.sample_rate, Some(22_050));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1));
    assert!(params.extra_data.is_none());
    assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(22_050));

    let packets = read_packets(reader.as_mut());
    assert!(packets.iter().all(|(_, dur, _)| *dur == 1024));
}

#[test]
fn adts_decode_with_detected_sbr_equals_lazy_detection() {
    // A decoder that is only told about the core codec finds the extensions in the stream. It must
    // output the same audio as a decoder that was configured from the detected extensions.
    for (name, data, channels) in [
        ("he-aac v1 stereo", HE_AAC_V1_STEREO, 2),
        ("he-aac v1 mono", HE_AAC_V1_MONO, 1),
        ("he-aac v2", HE_AAC_V2, 1),
    ] {
        let mut reader = AdtsReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (_, params) = track_params(reader.as_ref());
        let packets = read_packets(reader.as_mut());

        let mut configured = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let (formats_a, samples_a) = decode_packets(&mut configured, &packets);

        let mut core_params = AudioCodecParameters::new();
        core_params.for_codec(CODEC_ID_AAC).with_sample_rate(22_050).with_channels(
            if channels == 2 {
                layouts::CHANNEL_LAYOUT_STEREO
            }
            else {
                layouts::CHANNEL_LAYOUT_MONO
            },
        );

        let mut lazy = AacDecoder::try_new(&core_params, &AudioDecoderOptions::default()).unwrap();
        let (formats_b, samples_b) = decode_packets(&mut lazy, &packets);

        assert_eq!(formats_a, formats_b, "{name}");
        assert_eq!(samples_a, samples_b, "{name}");
        assert!(samples_a.iter().any(|s| s.abs() > 1e-4), "{name}: silence");
    }
}

/// Builds a LOAS stream (audioMuxVersion 0, one raw data block per frame) of AAC-LC at 22.05 kHz
/// that does not signal SBR, carrying the raw data blocks. `channel_config` is the channel
/// configuration of the audio specific config, or 0 for the channel layout of a program config
/// with front elements `front` (`true` for a channel pair element).
fn loas_stream_22k(channel_config: u32, front: &[bool], blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut stream = vec![];

    for (i, block) in blocks.iter().enumerate() {
        let mut bw = BitWriter::default();

        // useSameStreamMux
        bw.put((i != 0) as u32, 1);

        if i == 0 {
            bw.put(0, 1); // audioMuxVersion
            bw.put(1, 1); // allStreamsSameTimeFraming
            bw.put(0, 6); // numSubFrames
            bw.put(0, 4); // numProgram - 1
            bw.put(0, 3); // numLayer - 1
            // AudioSpecificConfig: AAC-LC, 22.05 kHz. It starts at a byte boundary of the frame.
            bw.put(2, 5);
            bw.put(7, 4);
            bw.put(channel_config, 4);
            bw.put(0, 3);

            if channel_config == 0 {
                // program_config_element(): front elements only, no mixdown, no comment.
                bw.put(0, 4); // element_instance_tag
                bw.put(1, 2); // object_type: AAC-LC
                bw.put(7, 4); // sampling_frequency_index
                bw.put(front.len() as u32, 4);
                bw.put(0, 4); // num_side_channel_elements
                bw.put(0, 4); // num_back_channel_elements
                bw.put(0, 2); // num_lfe_channel_elements
                bw.put(0, 3); // num_assoc_data_elements
                bw.put(0, 4); // num_valid_cc_elements
                bw.put(0, 3); // mixdown flags

                for (tag, &is_cpe) in front.iter().enumerate() {
                    bw.put(u32::from(is_cpe), 1);
                    bw.put(tag as u32, 4);
                }

                bw.align(); // relative to the start of the audio specific config
                bw.put(0, 8); // comment_field_bytes
            }

            bw.put(0, 3); // frameLengthType
            bw.put(0xff, 8); // latmBufferFullness
            bw.put(0, 1); // otherDataPresent
            bw.put(0, 1); // crcCheckPresent
        }

        // PayloadLengthInfo
        let mut len = block.len();
        while len >= 255 {
            bw.put(255, 8);
            len -= 255;
        }
        bw.put(len as u32, 8);

        for &byte in block {
            bw.put(u32::from(byte), 8);
        }

        let len = bw.bytes.len();
        stream.extend_from_slice(&[0x56, 0xe0 | (len >> 8) as u8, len as u8]);
        stream.extend_from_slice(&bw.bytes);
    }

    stream
}

fn blocks_of(data: &[u8]) -> Vec<Vec<u8>> {
    let mut adts = AdtsReader::try_probe_new(mss_of(data), Default::default()).unwrap();
    read_packets(adts.as_mut()).into_iter().map(|(_, _, data)| data).collect()
}

#[test]
fn loas_reports_implicit_sbr_and_ps_in_params_and_timeline() {
    for (name, data, channel_config, channels) in [
        ("he-aac v1 stereo", HE_AAC_V1_STEREO, 2, 2),
        ("he-aac v1 mono", HE_AAC_V1_MONO, 1, 1),
        ("he-aac v2", HE_AAC_V2, 1, 2),
    ] {
        // The raw data blocks of the ADTS fixture, in a LOAS stream that does not signal SBR.
        let blocks = blocks_of(data);

        let adts = AdtsReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (_, adts_params) = track_params(adts.as_ref());

        let stream = loas_stream_22k(channel_config, &[], &blocks);
        let mut reader = LoasReader::try_probe_new(mss_of(&stream), Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(44_100), "{name}");

        let packets = read_packets(reader.as_mut());
        assert_eq!(packets.len(), blocks.len(), "{name}");

        for (i, (pts, dur, data)) in packets.iter().enumerate() {
            assert_eq!(*dur, 2048, "{name}");
            assert_eq!(*pts, i as i64 * 2048, "{name}");
            assert_eq!(data, &blocks[i], "{name}");
        }

        // The same audio as the ADTS stream.
        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let (_, from_loas) = decode_packets(&mut decoder, &packets);

        let mut decoder =
            AacDecoder::try_new(&adts_params, &AudioDecoderOptions::default()).unwrap();
        let (_, from_adts) = decode_packets(&mut decoder, &packets);

        assert_eq!(from_loas, from_adts, "{name}");
    }
}

/// The same as for a predefined channel configuration, when the channel layout is that of a
/// program config element (channel configuration 0): the explicit signalling of the SBR that is
/// detected has to rewrite the program config, whose byte alignment moves.
#[test]
fn loas_with_a_program_config_reports_implicit_sbr_and_ps() {
    for (name, data, front, channels) in [
        ("he-aac v1 stereo", HE_AAC_V1_STEREO, vec![true], 2),
        ("he-aac v1 mono", HE_AAC_V1_MONO, vec![false], 1),
        ("he-aac v2", HE_AAC_V2, vec![false], 2),
    ] {
        let blocks = blocks_of(data);

        let adts = AdtsReader::try_probe_new(mss_of(data), Default::default()).unwrap();
        let (_, adts_params) = track_params(adts.as_ref());

        let stream = loas_stream_22k(0, &front, &blocks);
        let mut reader = LoasReader::try_probe_new(mss_of(&stream), Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
        assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(44_100), "{name}");

        let packets = read_packets(reader.as_mut());
        assert_eq!(packets.len(), blocks.len(), "{name}");
        assert!(packets.iter().all(|(_, dur, _)| *dur == 2048), "{name}");

        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let (formats, from_loas) = decode_packets(&mut decoder, &packets);
        assert!(formats.iter().all(|&f| f == (44_100, channels, 2048)), "{name}");

        let mut decoder =
            AacDecoder::try_new(&adts_params, &AudioDecoderOptions::default()).unwrap();
        let (_, from_adts) = decode_packets(&mut decoder, &packets);

        assert_eq!(from_loas, from_adts, "{name}");
    }
}

#[test]
fn loas_without_sbr_keeps_the_core_rate_and_timeline() {
    let stream = loas_stream_22k(1, &[], &blocks_of(AAC_LC_22K_MONO));
    let mut reader = LoasReader::try_probe_new(mss_of(&stream), Default::default()).unwrap();
    let (track, params) = track_params(reader.as_ref());

    assert_eq!(params.sample_rate, Some(22_050));
    assert_eq!(track.time_base.map(|tb| tb.denom.get()), Some(22_050));
    assert!(read_packets(reader.as_mut()).iter().all(|(_, dur, _)| *dur == 1024));
}

#[test]
fn adts_seek_with_implicit_sbr_uses_decoded_frames() {
    let mut reader =
        AdtsReader::try_probe_new(mss_of(HE_AAC_V1_STEREO), Default::default()).unwrap();

    // Seek to frame 20 of 24: the decoder starts at the multiple of 16 frames at least 41 frames
    // before the target, that is the start of the stream.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(20 * 2048 + 5), track_id: 0 },
        )
        .unwrap();

    assert_eq!(seeked.required_ts.get(), 20 * 2048 + 5);
    assert_eq!(seeked.actual_ts.get(), 0);

    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts.get(), 0);
    assert_eq!(packet.dur.get(), 2048);
}

/// The generated format-coverage samples are used if `RMPD_SAMPLES` points at them.
fn sample(name: &str) -> Option<MediaSourceStream<'static>> {
    let dir = std::env::var_os("RMPD_SAMPLES")?;
    let file = std::fs::File::open(std::path::Path::new(&dir).join("aac").join(name)).ok()?;
    Some(MediaSourceStream::new(Box::new(file), Default::default()))
}

#[test]
fn sample_adts_he_aac_decodes_to_the_reported_output() {
    for (name, channels) in [("heaac_v1_fdk_adts.aac", 2), ("heaac_v2_fdk_adts.aac", 2)] {
        let Some(mss) = sample(name)
        else {
            return;
        };

        let mut reader = AdtsReader::try_probe_new(mss, Default::default()).unwrap();
        let (track, params) = track_params(reader.as_ref());

        assert_eq!(params.sample_rate, Some(44_100), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");

        // The estimated duration is on the timeline of decoded frames: within 1% of the real one.
        let duration = track.duration.unwrap().get();
        let packets = read_packets(reader.as_mut());
        let real = packets.last().map(|(pts, dur, _)| *pts as u64 + dur).unwrap();
        assert!(duration.abs_diff(real) * 100 < real, "{name}: {duration} vs {real}");

        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let (formats, _) = decode_packets(&mut decoder, &packets);
        let frames: usize = formats.iter().map(|f| f.2).sum();

        assert_eq!(frames as u64, real, "{name}");
        assert!(formats.iter().all(|f| f.0 == 44_100 && f.1 == channels), "{name}");
    }
}
