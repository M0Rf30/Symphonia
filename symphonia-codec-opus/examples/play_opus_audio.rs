// Opus Audio Player - Decode and play Opus files with audio output
// Uses cpal for cross-platform audio output

use std::env;
use std::fs::File;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender};

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use symphonia_codec_opus::OpusDecoder;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <opus_file>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];

    println!("Opus Audio Player");
    println!("=================");
    println!("File: {}\n", path);

    // Open the file
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("✗ Failed to open file: {}", e);
            std::process::exit(1);
        }
    };

    // Create media source
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create hint for format detection
    let mut hint = Hint::new();
    if let Some(ext) = path.split('.').last() {
        hint.with_extension(ext);
    }

    // Probe the file
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();

    let probed = match symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ Failed to probe file: {}", e);
            std::process::exit(1);
        }
    };

    let mut format = probed.format;

    // Get the default track
    let track = match format.default_track() {
        Some(t) => t,
        None => {
            eprintln!("✗ No tracks found");
            std::process::exit(1);
        }
    };

    // Verify it's Opus
    if track.codec_params.codec != CODEC_TYPE_OPUS {
        eprintln!("✗ Not an Opus codec (found {:?})", track.codec_params.codec);
        std::process::exit(1);
    }

    // Print track info
    let sample_rate = track.codec_params.sample_rate.unwrap_or(48000);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    println!("Track Information:");
    println!("  Codec: Opus");
    println!("  Sample Rate: {} Hz", sample_rate);
    println!("  Channels: {}", channels);

    if let Some(n_frames) = track.codec_params.n_frames {
        let duration_secs = n_frames as f64 / sample_rate as f64;
        println!("  Duration: {:.2} seconds", duration_secs);
    }
    println!();

    // Create decoder (using our custom OpusDecoder)
    let dec_opts = DecoderOptions::default();
    let mut decoder: Box<dyn Decoder> = match OpusDecoder::try_new(&track.codec_params, &dec_opts) {
        Ok(d) => Box::new(d),
        Err(e) => {
            eprintln!("✗ Failed to create decoder: {}", e);
            std::process::exit(1);
        }
    };

    println!("✓ Decoder created");

    // Set up audio output
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            eprintln!("✗ No audio output device available");
            std::process::exit(1);
        }
    };

    println!("✓ Audio device: {}", device.name().unwrap_or_else(|_| "Unknown".to_string()));

    // Get device config
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ Failed to get output config: {}", e);
            std::process::exit(1);
        }
    };

    println!("✓ Output format: {} Hz, {} channels, {:?}",
             config.sample_rate().0, config.channels(), config.sample_format());

    // Create audio buffer for playback
    let audio_buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
    let audio_buffer_clone = audio_buffer.clone();

    // Create channel for signaling when playback is done
    let (tx, rx) = channel();
    let tx_clone = tx.clone();

    // Build output stream
    let sample_format = config.sample_format();
    let stream_config = config.into();

    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, audio_buffer_clone, tx_clone),
        SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, audio_buffer_clone, tx_clone),
        SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, audio_buffer_clone, tx_clone),
        _ => {
            eprintln!("✗ Unsupported sample format: {:?}", sample_format);
            std::process::exit(1);
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ Failed to create audio stream: {}", e);
            std::process::exit(1);
        }
    };

    println!("✓ Audio stream created\n");

    // Start playback BEFORE decoding to ensure stream is ready
    if let Err(e) = stream.play() {
        eprintln!("✗ Failed to start playback: {}", e);
        std::process::exit(1);
    }

    println!("Playing audio...\n");

    // Decode and play
    let mut packet_count = 0;
    let mut decode_errors = 0;
    let mut total_samples = 0;

    loop {
        // Get next packet
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                println!("\n✓ Reached end of file");
                break;
            }
            Err(e) => {
                eprintln!("\n✗ Error reading packet: {}", e);
                decode_errors += 1;
                continue;
            }
        };

        // Decode packet
        match decoder.decode(&packet) {
            Ok(decoded) => {
                // Convert to f32 and add to playback buffer
                let samples = convert_audio_buffer(&decoded);
                total_samples += samples.len();

                // Add to buffer
                let mut buffer = audio_buffer.lock().unwrap();
                buffer.extend_from_slice(&samples);
                let buffer_len = buffer.len();
                drop(buffer);

                packet_count += 1;
                if packet_count % 50 == 0 {
                    print!("\r  Packets: {}  Decoded: {} samples  Buffer: {} samples  ",
                           packet_count, total_samples, buffer_len);
                    std::io::Write::flush(&mut std::io::stdout()).unwrap();
                }
            }
            Err(e) => {
                eprintln!("\n✗ Decode error: {}", e);
                decode_errors += 1;
            }
        }
    }

    println!("\n\n✓ Decoding complete");
    println!("  Total packets: {}", packet_count);
    println!("  Total samples decoded: {}", total_samples);
    println!("  Decode errors: {}", decode_errors);

    // Check if we have buffered samples
    let remaining = audio_buffer.lock().unwrap().len();
    println!("  Buffered samples: {}", remaining);

    if total_samples == 0 {
        println!("\n⚠ Warning: No audio samples were decoded!");
    } else {
        println!("\nWaiting for playback to finish...");

        // Signal end of stream
        drop(tx);

        // Wait for playback to complete (or timeout after 10 seconds)
        use std::time::Duration;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(_) => println!("✓ Playback complete"),
            Err(_) => println!("⚠ Playback timeout - buffer may not be empty"),
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    done_signal: Sender<()>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut done_sent = false;
    let mut samples_played = 0usize;
    let mut underruns = 0usize;

    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut buffer = audio_buffer.lock().unwrap();
            let buffer_size = buffer.len();

            for frame in data.chunks_mut(channels) {
                if buffer.is_empty() {
                    // Buffer underrun - output silence
                    underruns += 1;
                    if !done_sent {
                        if buffer_size == 0 && samples_played > 0 {
                            // Only signal done if we've played something
                            let _ = done_signal.send(());
                            done_sent = true;
                        }
                    }
                    for sample in frame.iter_mut() {
                        *sample = T::EQUILIBRIUM;
                    }
                } else {
                    // Output samples
                    for sample in frame.iter_mut() {
                        let value = buffer.remove(0);
                        *sample = cpal::Sample::from_sample(value);
                        samples_played += 1;
                    }
                }
            }
        },
        move |err| {
            eprintln!("Audio stream error: {}", err);
        },
        None,
    )
}

fn convert_audio_buffer(audio: &AudioBufferRef) -> Vec<f32> {
    match audio {
        AudioBufferRef::F32(buf) => {
            let channels = buf.spec().channels.count();
            let frames = buf.frames();
            let mut samples = Vec::with_capacity(frames * channels);

            // Interleave channels
            for i in 0..frames {
                for ch in 0..channels {
                    samples.push(buf.chan(ch)[i]);
                }
            }
            samples
        }
        _ => {
            eprintln!("✗ Unsupported audio buffer format");
            Vec::new()
        }
    }
}
