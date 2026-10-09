// Decode a DSF/DFF file to PCM and print an FNV-1a hash of the output samples.
//
// Usage: dump_pcm <file.dsf|file.dff> <pcm_rate> [max_packets] [out.f32]
//
// The hash (and the optional raw little-endian f32 dump) makes it easy to compare the decoder
// output of two builds.
use std::env;
use std::fs::File;
use std::io::Write;

use symphonia_codec_dsd::DsdDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <file.dsf|file.dff> <pcm_rate> [max_packets] [out.f32]", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];
    let pcm_rate: u32 = args[2].parse().expect("pcm_rate");
    let max_packets: usize = args.get(3).map_or(usize::MAX, |s| s.parse().expect("max_packets"));
    let mut dump = args.get(4).map(|p| File::create(p).expect("create dump"));

    let mss = MediaSourceStream::new(Box::new(File::open(path).expect("open")), Default::default());
    let mut reader: Box<dyn FormatReader> = if path.ends_with(".dff") {
        Box::new(
            symphonia_format_dsd::DffReader::try_new(mss, FormatOptions::default()).expect("dff"),
        )
    }
    else {
        Box::new(
            symphonia_format_dsd::DsfReader::try_new(mss, FormatOptions::default()).expect("dsf"),
        )
    };

    let (track_id, params) = reader
        .tracks()
        .iter()
        .find_map(|t| match t.codec_params.as_ref() {
            Some(CodecParameters::Audio(a)) => Some((t.id, a.clone())),
            _ => None,
        })
        .expect("no audio track");

    let mut params = params;
    params.extra_data = Some(pcm_rate.to_le_bytes().to_vec().into_boxed_slice());
    let mut decoder =
        DsdDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder");

    let mut hash: u64 = 0xcbf29ce484222325;
    let mut total = 0usize;
    let mut packets = 0usize;

    while packets < max_packets {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            _ => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        let GenericAudioBufferRef::F32(buf) =
            decoder.decode_ref(&packet.as_packet_ref()).expect("decode")
        else {
            panic!("expected F32 output");
        };
        packets += 1;
        for ch in 0..buf.spec().channels().count() {
            for &s in buf.plane(ch).expect("plane") {
                for b in s.to_bits().to_le_bytes() {
                    hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
                if let Some(f) = dump.as_mut() {
                    f.write_all(&s.to_le_bytes()).unwrap();
                }
                total += 1;
            }
        }
    }

    println!("{path} rate={pcm_rate} packets={packets} samples={total} fnv1a={hash:016x}");
}
