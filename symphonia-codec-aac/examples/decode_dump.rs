//! Decodes an AAC file and writes the decoded audio as interleaved little-endian f32 samples.
//!
//! Usage: `decode_dump <input> <output.f32> [adts|loas|adif|mp4]`
//!
//! Prints the codec parameters, the timeline of the track, and the decoded format to stderr. This
//! is a development aid used to compare the decoder with other decoders.

use std::fs::File;
use std::io::{BufWriter, Write};

use symphonia_codec_aac::{AacDecoder, AdifReader, AdtsReader, LoasReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_isomp4::IsoMp4Reader;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = &args[1];
    let output = &args[2];
    let kind = args.get(3).map(String::as_str).unwrap_or("mp4");

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

    let mut out = BufWriter::new(File::create(output).expect("create output"));
    let mut total = 0usize;
    let mut n_packets = 0usize;
    let mut last_end = None;

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
            }
            Err(err) => eprintln!("decode error at packet {n_packets}: {err}"),
        }
    }

    eprintln!("packets={n_packets} frames={total} timeline_end={last_end:?}");
}
