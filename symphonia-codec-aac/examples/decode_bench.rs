//! Times the decoding of an AAC file (the packets are read first, so only the decoder is timed).
//!
//! Usage: `decode_bench <input> [adts|loas|mp4] [repetitions]`
//!
//! Prints the best and the median time of the repetitions and the real-time factor.

use std::fs::File;
use std::time::Instant;

use symphonia_codec_aac::{AacDecoder, AdtsReader, LoasReader};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::formats::{FormatReader, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_isomp4::IsoMp4Reader;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = args.get(2).map(String::as_str).unwrap_or("mp4");
    let reps: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    let mss =
        MediaSourceStream::new(Box::new(File::open(&args[1]).expect("open")), Default::default());

    let mut reader: Box<dyn FormatReader> = match kind {
        "adts" => AdtsReader::try_probe_new(mss, Default::default()).expect("adts"),
        "loas" => LoasReader::try_probe_new(mss, Default::default()).expect("loas"),
        _ => IsoMp4Reader::try_probe_new(mss, Default::default()).expect("mp4"),
    };

    let track = reader.default_track(TrackType::Audio).expect("track");
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };

    let mut packets = vec![];
    while let Some(packet) = reader.next_packet().expect("packet") {
        if packet.track_id == id {
            packets.push(packet);
        }
    }

    let mut times = vec![];
    let mut frames = 0usize;
    let mut rate = 0u32;

    for _ in 0..reps {
        let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        frames = 0;
        let start = Instant::now();
        for packet in &packets {
            let buf = decoder.decode(packet).expect("decode");
            frames += buf.frames();
            rate = buf.spec().rate();
        }
        times.push(start.elapsed().as_secs_f64());
    }

    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let best = times[0];
    let median = times[times.len() / 2];
    let secs = frames as f64 / f64::from(rate);

    println!(
        "{}: best {:.1} ms, median {:.1} ms, {:.0}x real time ({:.1} s of audio at {} Hz)",
        args[1],
        best * 1e3,
        median * 1e3,
        secs / best,
        secs,
        rate
    );
}
