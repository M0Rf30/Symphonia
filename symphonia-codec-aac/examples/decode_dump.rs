//! Decodes an AAC file and writes the decoded audio as interleaved little-endian f32 samples.
//!
//! Usage: `decode_dump <input> <output.f32> [adts|loas|adif|mp4]`
//!
//! Prints the codec parameters, the timeline of the track, and the decoded format to stderr. This
//! is a development aid used to compare the decoder with other decoders.

use std::fs::File;
use std::io::{BufWriter, Write};

use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use symphonia_codec_aac::{AacDecoder, AdifReader, AdtsReader, LoasReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_isomp4::IsoMp4Reader;

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        eprintln!("[{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

fn main() {
    if std::env::var_os("LOG").is_some() {
        log::set_logger(&StderrLogger).unwrap();
        log::set_max_level(log::LevelFilter::Debug);
    }

    let args: Vec<String> = std::env::args().collect();
    let input = &args[1];
    let output = &args[2];
    let kind = args.get(3).map(String::as_str).unwrap_or("mp4");

    if kind == "bin" {
        decode_bin(input, output);
        return;
    }

    let file = File::open(input).expect("open input");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut reader: Box<dyn FormatReader> = match kind {
        "adts" => AdtsReader::try_probe_new(mss, Default::default()).expect("adts"),
        "loas" => LoasReader::try_probe_new(mss, Default::default()).expect("loas"),
        "adif" => AdifReader::try_probe_new(mss, Default::default()).expect("adif"),
        _ => IsoMp4Reader::try_probe_new(mss, Default::default()).expect("mp4"),
    };

    let track = reader.tracks()[0].clone();
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => panic!("no audio parameters"),
    };

    eprintln!(
        "params: rate={:?} channels={:?} profile={:?} time_base={:?} duration={:?} num_frames={:?} delay={:?} padding={:?}",
        params.sample_rate,
        params.channels.as_ref().map(|c| c.count()),
        params.profile,
        track.time_base,
        track.duration,
        track.num_frames,
        track.delay,
        track.padding
    );

    let mut opts = AudioDecoderOptions::default();
    opts.gapless = std::env::var_os("GAPLESS").is_some();
    let mut decoder = AacDecoder::try_new(&params, &opts).expect("decoder");

    // Optionally write the audio specific config and the access units as a `bin` file.
    let mut bin = std::env::var_os("DUMP_BIN").map(|path| {
        let mut bin = BufWriter::new(File::create(path).expect("create bin"));
        let asc = params.extra_data.as_deref().expect("audio specific config");
        bin.write_all(&(asc.len() as u16).to_be_bytes()).unwrap();
        bin.write_all(asc).unwrap();
        bin
    });

    let mut out = BufWriter::new(File::create(output).expect("create output"));
    let mut total = 0usize;
    let mut n_packets = 0usize;
    let mut last_end = None;
    let mut last_frames = 0;
    let mut last_channels = 0;

    while let Some(packet) = reader.next_packet().expect("next packet") {
        if n_packets < 3 {
            eprintln!(
                "packet pts={} dur={} trim_start={} trim_end={} len={}",
                packet.pts.get(),
                packet.dur.get(),
                packet.trim_start.get(),
                packet.trim_end.get(),
                packet.data.len()
            );
        }
        n_packets += 1;
        last_end = Some(packet.pts.get() + packet.dur.get() as i64);

        if let Some(bin) = bin.as_mut() {
            bin.write_all(&(packet.data.len() as u16).to_be_bytes()).unwrap();
            bin.write_all(&packet.data).unwrap();
        }

        match decoder.decode(&packet) {
            Ok(buf) => {
                if total == 0 {
                    eprintln!(
                        "decoded: rate={} channels={}",
                        buf.spec().rate(),
                        buf.spec().channels().count()
                    );
                }
                let GenericAudioBufferRef::F32(buf) = buf
                else {
                    panic!("expected f32");
                };
                let n_ch = buf.spec().channels().count();
                for i in 0..buf.frames() {
                    for c in 0..n_ch {
                        out.write_all(&buf.plane(c).unwrap()[i].to_le_bytes()).unwrap();
                    }
                }
                total += buf.frames();
                last_frames = buf.frames();
                last_channels = n_ch;
            }
            Err(err) => {
                eprintln!("decode error at packet {n_packets}: {err}");

                // Keep the output aligned with the stream: write silence for the lost frames.
                if std::env::var_os("ZERO_ON_ERROR").is_some() {
                    for _ in 0..last_frames * last_channels {
                        out.write_all(&0f32.to_le_bytes()).unwrap();
                    }
                }
            }
        }
    }

    eprintln!("packets={n_packets} frames={total} timeline_end={last_end:?}");
}

/// Decode a file of an audio specific config and access units.
fn decode_bin(input: &str, output: &str) {
    let data = std::fs::read(input).expect("read input");

    fn next(data: &[u8], pos: &mut usize) -> Vec<u8> {
        let len = usize::from(u16::from_be_bytes([data[*pos], data[*pos + 1]]));
        let record = data[*pos + 2..*pos + 2 + len].to_vec();
        *pos += 2 + len;
        record
    }

    let mut pos = 0;
    let asc_bytes = next(&data, &mut pos);
    let asc = AudioSpecificConfig::read(&asc_bytes).expect("audio specific config");

    let mut params = AudioCodecParameters::new();

    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_extra_data(asc_bytes.into_boxed_slice());

    if let Some(channels) = asc.output_channels() {
        params.with_channels(channels);
    }

    eprintln!(
        "asc: {:?} rate={} samples={} channels={:?} sbr={} eld_sbr={:?}",
        asc.object_type,
        asc.sample_rate,
        asc.samples,
        asc.channels.as_ref().map(|c| c.count()),
        asc.sbr_present,
        asc.eld_sbr
    );

    let mut decoder =
        AacDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder");

    let mut out = BufWriter::new(File::create(output).expect("create output"));
    let mut n = 0i64;
    let mut format = None;

    while pos < data.len() {
        let au = next(&data, &mut pos);
        let packet = Packet::new(
            0,
            Timestamp::new(n),
            Duration::new(asc.samples as u64),
            au.into_boxed_slice(),
        );
        n += asc.samples as i64;

        match decoder.decode(&packet) {
            Ok(GenericAudioBufferRef::F32(buf)) => {
                let n_ch = buf.spec().channels().count();
                format = Some((buf.spec().rate(), n_ch, buf.frames()));
                for i in 0..buf.frames() {
                    for c in 0..n_ch {
                        out.write_all(&buf.plane(c).unwrap()[i].to_le_bytes()).unwrap();
                    }
                }
            }
            Ok(_) => panic!("expected f32"),
            Err(err) => eprintln!("decode error at packet {}: {err}", n / asc.samples as i64),
        }
    }

    eprintln!("decoded format (rate, channels, frames per packet): {format:?}");
}
