// DSD Codec Benchmarks

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use symphonia_codec_dsd::{DsdDecoder, CODEC_ID_DSD};
use symphonia_core::audio::Channels;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Benchmark CIC filter processing
fn bench_cic_filter(c: &mut Criterion) {
    use symphonia_codec_dsd::cic::CicFilter;

    let mut group = c.benchmark_group("cic_filter");

    for decimation in [8, 16, 32, 64].iter() {
        group.throughput(Throughput::Elements(8192));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("decimation_{}", decimation)),
            decimation,
            |b, &decimation| {
                let mut filter = CicFilter::new(decimation, 4);
                let input = vec![0.5f32; 8192];
                let mut output = vec![0.0f32; 8192 / decimation];

                b.iter(|| {
                    filter.reset();
                    black_box(filter.process_buffer(&input, &mut output))
                });
            },
        );
    }

    group.finish();
}

/// Benchmark FIR filter processing (tests SIMD optimization)
fn bench_fir_filter(c: &mut Criterion) {
    use symphonia_codec_dsd::fir::FirDecimator;

    let mut group = c.benchmark_group("fir_filter");

    for (decimation, taps) in [(2, 63), (4, 31), (8, 31)].iter() {
        group.throughput(Throughput::Elements(8192));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("dec{}_taps{}", decimation, taps)),
            &(*decimation, *taps),
            |b, &(decimation, taps)| {
                let mut filter = FirDecimator::new(decimation, taps, 0.4);
                let input = vec![0.5f32; 8192];
                let mut output = vec![0.0f32; 8192 / decimation];

                b.iter(|| {
                    filter.reset();
                    black_box(filter.process_buffer(&input, &mut output))
                });
            },
        );
    }

    group.finish();
}

/// Benchmark full DSD-to-PCM decimation pipeline
fn bench_dsd_decimation(c: &mut Criterion) {
    use symphonia_codec_dsd::decimator::{DecimationConfig, DsdDecimator};
    use symphonia_core::codecs::audio::BitOrder;

    let mut group = c.benchmark_group("dsd_decimation");

    let configs = [
        ("DSD64_to_44k", 2822400, 44100),
        ("DSD64_to_88k", 2822400, 88200),
        ("DSD128_to_44k", 5644800, 44100),
        ("DSD128_to_88k", 5644800, 88200),
    ];

    for (name, dsd_rate, pcm_rate) in configs.iter() {
        let config = DecimationConfig::new(*dsd_rate, *pcm_rate).unwrap();
        let bytes_per_packet = 4096;

        group.throughput(Throughput::Bytes(bytes_per_packet as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &config,
            |b, config| {
                let mut decimator = DsdDecimator::new(*config, 2, BitOrder::LsbFirst);
                let dsd_input = vec![0x55u8; bytes_per_packet];
                let dsd_planes = vec![dsd_input.as_slice(); 2];

                let output_samples = (bytes_per_packet * 8) / config.total_decimation;
                let mut pcm_output = vec![vec![0.0f32; output_samples]; 2];
                let mut pcm_planes: Vec<&mut [f32]> = pcm_output.iter_mut()
                    .map(|v| v.as_mut_slice())
                    .collect();

                b.iter(|| {
                    decimator.reset();
                    black_box(decimator.process_planar(&dsd_planes, &mut pcm_planes).unwrap())
                });
            },
        );
    }

    group.finish();
}

/// Benchmark bitstream unpacking
fn bench_bitstream_unpacking(c: &mut Criterion) {
    use symphonia_codec_dsd::bitstream::unpack_dsd_bytes_to_f32;
    use symphonia_core::codecs::audio::BitOrder;

    let mut group = c.benchmark_group("bitstream_unpacking");

    for size in [1024, 4096, 16384].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_bytes", size)),
            size,
            |b, &size| {
                let input = vec![0xAAu8; size];
                let mut output = vec![0.0f32; size * 8];

                b.iter(|| {
                    black_box(unpack_dsd_bytes_to_f32(&input, BitOrder::LsbFirst, &mut output))
                });
            },
        );
    }

    group.finish();
}

/// Benchmark full decoder (pass-through mode)
fn bench_decoder_passthrough(c: &mut Criterion) {
    let mut group = c.benchmark_group("decoder_passthrough");

    let packet_sizes = [4096, 8192, 16384];

    for &size in packet_sizes.iter() {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_bytes", size)),
            &size,
            |b, &size| {
                let mut params = AudioCodecParameters::new();
                params
                    .for_codec(CODEC_ID_DSD)
                    .with_sample_rate(2822400)
                    .with_channels(Channels::Discrete(2))
                    .with_max_frames_per_packet(size as u64);

                let mut decoder = DsdDecoder::try_new(&params, &AudioDecoderOptions::default())
                    .unwrap();

                let data = vec![0x55u8; size];
                let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(size as u64), &data);

                b.iter(|| {
                    decoder.reset();
                    black_box(decoder.decode_ref(&packet).unwrap().frames())
                });
            },
        );
    }

    group.finish();
}

/// Benchmark full decoder (PCM conversion mode)
fn bench_decoder_pcm(c: &mut Criterion) {
    let mut group = c.benchmark_group("decoder_pcm");

    let configs = [
        ("DSD64_to_44k", 2822400, 44100),
        ("DSD64_to_88k", 2822400, 88200),
    ];

    for (name, dsd_rate, pcm_rate) in configs.iter() {
        let packet_size = 4096usize;
        group.throughput(Throughput::Bytes(packet_size as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(*dsd_rate, *pcm_rate, packet_size),
            |b, &(dsd_rate, pcm_rate, packet_size)| {
                let mut params = AudioCodecParameters::new();
                params
                    .for_codec(CODEC_ID_DSD)
                    .with_sample_rate(dsd_rate)
                    .with_channels(Channels::Discrete(2))
                    .with_max_frames_per_packet(packet_size as u64);

                // Enable PCM mode
                let extra_data = (pcm_rate as u32).to_le_bytes().to_vec().into_boxed_slice();
                params.extra_data = Some(extra_data);

                let mut decoder = DsdDecoder::try_new(&params, &AudioDecoderOptions::default())
                    .unwrap();

                let data = vec![0xAAu8; packet_size]; // Alternating pattern
                let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(packet_size as u64), &data);

                b.iter(|| {
                    decoder.reset();
                    black_box(decoder.decode_ref(&packet).unwrap().frames())
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_cic_filter,
    bench_fir_filter,
    bench_dsd_decimation,
    bench_bitstream_unpacking,
    bench_decoder_passthrough,
    bench_decoder_pcm
);
criterion_main!(benches);
