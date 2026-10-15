# Fuzzing in Symphonia

This directory contains the fuzzing infrastructure for Symphonia, utilizing `cargo-fuzz` and `libfuzzer-sys`.

## Targets

### General

* **`decode_any`**: Fuzzes the full pipeline (Probe -> Demuxer -> Decoder). Good for container formats.

### Component Decoder Fuzzers

These targets bypass the demuxer and feed data directly to the codec implementation, maximizing decoder coverage.

* **`decode_mpa`**: MPEG Audio Layer 1/2/3
* **`decode_flac`**: FLAC
* **`decode_aac`**: AAC (LC)
* **`decode_vorbis`**: Vorbis
* **`decode_pcm`**: PCM (All supported integer/float formats)
* **`decode_adpcm`**: ADPCM (Microsoft, IMA-WAV, IMA-QT)
* **`decode_alac`**: Apple Lossless (ALAC)
* **`decode_ape`**: Monkey's Audio (APE)
* **`decode_musepack`**: Musepack
* **`decode_opus`**: Opus
* **`decode_wavpack`**: WavPack
* **`decode_dsd`**: DSD
* **`decode_ac3`**: AC-3 and E-AC-3
* **`decode_dca`**: DTS
* **`decode_truehd`**: Dolby TrueHD and MLP
* **`decode_speex`**: Speex (with and without an Ogg header)
* **`decode_nellymoser`**: Nellymoser
* **`decode_wma`**: Windows Media Audio 1 and 2 (the first byte selects the version, channels,
  stream flags and sample rate)
* **`decode_sbc`**, **`decode_aptx`**: SBC/mSBC, and aptX/aptX HD (the first byte selects)
* **`decode_lc3`**: LC3 and LC3plus (the first two bytes select the sample rate, frame duration,
  high resolution mode and channels)
* **`decode_dolby_e`**: Dolby E

The last five targets read the input as a sequence of packets, each prefixed by its
16-bit little-endian length (`fuzz_audio_decoder_packets`), so state carried between packets is
exercised. A first selector byte, where present, picks the codec variant or stream parameters.

### Component Demuxer Fuzzers

These targets instantiate a specific `FormatReader` directly, bypassing probing. This ensures coverage of demuxing logic even if the probe would have failed or if specific header bytes are missing.

* **`demux_mkv`**: Matroska / WebM
* **`demux_isomp4`**: ISO/MP4
* **`demux_ogg`**: OGG
* **`demux_flac`**: FLAC container
* **`demux_wav`**: WAV / RIFF
* **`demux_aiff`**: AIFF
* **`demux_caf`**: CAF
* **`demux_mpegts`**: MPEG-TS
* **`demux_mpegps`**: MPEG-PS
* **`demux_flv`**: FLV
* **`demux_asf`**: ASF (`.wma`, `.asf`)
* **`demux_dsf`**: DSF
* **`demux_dff`**: DFF
* **`demux_mp3`**: MPEG Audio (MP1/MP2/MP3) elementary stream
* **`demux_ape`**: Monkey's Audio (APE)
* **`demux_musepack`**: Musepack
* **`demux_wavpack`**: WavPack
* **`demux_adts`**: AAC ADTS
* **`demux_adif`**: AAC ADIF
* **`demux_loas`**: AAC LOAS/LATM
* **`demux_es`**: AC-3/E-AC-3, DTS, TrueHD/MLP and SMPTE ST 337 Dolby E elementary streams (first byte selects the reader)
* **`demux_bt`**: SBC, aptX, aptX HD and LC3 (first byte selects the reader)
* **`demux_spdif`**: IEC 61937 data bursts

Each target is a `[[bin]]` entry in `Cargo.toml` plus a file in `fuzz_targets/`. `run_all.sh` enumerates `fuzz_targets/*.rs`, so new targets only need those two.

CI (`.github/workflows/fuzz.yml`) runs every target for 10 seconds on each pull request and push,
and for 10 minutes each night (one job per target, crash inputs uploaded as artifacts).

## How to Run

From the `symphonia/fuzz` directory:

### Run a specific target

```bash
# Run the MPEG Audio fuzzer indefinitely
cargo fuzz run decode_mpa
```

### Run all targets (Smoke Test)

You can use the provided script to run all fuzzers sequentially for a set duration (default 10s) to verify they are working.

```bash
./run_all.sh [duration_in_seconds; default 10s]
```
