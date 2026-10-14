<div align="center">
<h1>Symphonia</h1>

<!--
<p>
    <img src="https://raw.githubusercontent.com/M0Rf30/Symphonia/rmpd-next/assets/logo.png" width="200px" />
</p>
-->

[<img alt="Crates.io" src="https://img.shields.io/crates/v/symphonia?style=for-the-badge" height="22"/>](https://crates.io/crates/symphonia)
[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia)
[<img alt="GitHub Actions Status" src="https://img.shields.io/github/actions/workflow/status/M0Rf30/Symphonia/ci.yml?branch=rmpd-next&style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/actions/workflows/ci.yml)
[<img alt="Deps.rs" src="https://deps.rs/repo/github/M0Rf30/Symphonia/status.svg?style=for-the-badge" height="22"/>](https://deps.rs/repo/github/M0Rf30/Symphonia)
[<img alt="Crates.io MSRV" src="https://img.shields.io/crates/msrv/symphonia?style=for-the-badge" height="22"/>](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/)
[<img alt="MPL License" src="https://img.shields.io/crates/l/symphonia?style=for-the-badge" height="22"/>](https://www.mozilla.org/en-US/MPL/2.0/)

<p>
    <strong>
        Symphonia is a pure Rust audio decoding and media demuxing library supporting AAC, AC-3, E-AC-3, ADPCM, AIFF, ALAC, APE, CAF, DSD, DTS, FLAC, FLV, MKV, MP1, MP2, MP3, mp3PRO, MP4, MPEG-PS, MPEG-TS, Musepack, Nellymoser, OGG, Opus, PCM, Speex, TrueHD, MLP, Vorbis, WAV, WavPack, and WebM.
    </strong>
</p>

<p>
    <h3>
        <a href="https://github.com/M0Rf30/Symphonia/blob/rmpd-next/GETTING_STARTED.md">Getting Started</a>
        <span> · </span>
        <a href="https://docs.rs/symphonia">Documentation</a>
        <span> · </span>
        <a href="https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia/examples">Examples</a>
        <span> · </span>
        <a href="https://github.com/M0Rf30/Symphonia/blob/rmpd-next/BENCHMARKS.md">Benchmarks</a>
    </h3>
</p>
</div>

---

## Features

* Decode support for the most popular audio codecs with support for gapless playback
* Demux the most common media container formats
* Read most metadata and tagging formats
* Automatic format and decoder detection
* Basic audio primitives for manipulating audio data efficiently
* 100% safe Rust
* Minimal dependencies
* Fast with no compromises in performance!

Additionally, planned features include:

* Providing a C API for integration into other languages
* Providing a WASM API for web usage

## Roadmap

Support for individual audio codecs and media formats are provided by separate crates. By default, Symphonia only enables support royalty-free open standard codecs and formats, but others may be enabled using feature flags.

> [!TIP]
> All formats and codecs can be enabled with the `all` feature flag.

### Status

The following status classifications are used to determine the state of development for each format or codec.

| Status    | Meaning                                                                                                                  |
|-----------|--------------------------------------------------------------------------------------------------------------------------|
| -         | In work or not started yet.                                                                                              |
| Good      | Many media streams play. Some streams may panic, error, or produce audible glitches. Some features may not be supported. |
| Great     | Most media streams play. Inaudible glitches may be present. Most common features are supported.                          |
| Excellent | All media streams play.  No audible or inaudible glitches. All required features are supported.                          |

A status of *Great* indicates that major development is complete and that the feature is in a state that would be acceptable for most applications to use.

A status of *Excellent* is only assigned after the feature passes all compliance tests. If no compliance tests are readily available, then a status of *Excellent* will be assigned if Symphonia's output matches that of a reference implementation, or `ffmpeg`, over a large test corpus.

### Formats (Demuxers)

| Format   | Status    | Gapless* | Feature Flag | Default | Crate                       |
|----------|-----------|----------|--------------|---------|-----------------------------|
| AIFF     | Excellent | Yes      | `aiff`       | No      | [`symphonia-format-riff`]   |
| APE      | Excellent | Yes      | `ape-format` | No      | [`symphonia-bundle-ape`]      |
| CAF      | Excellent | Yes      | `caf`        | No      | [`symphonia-format-caf`]    |
| DSD (DSF, DFF) | Great | Yes    | `dsd`        | No      | [`symphonia-format-dsd`]      |
| Elementary streams (AC-3, E-AC-3, DTS, TrueHD/MLP) | Great | Yes\*\* | `es`   | No      | [`symphonia-format-es`]       |
| FLV (audio only) | Great | Yes\*\*\* | `flv`      | No      | [`symphonia-format-flv`]    |
| ISO/MP4  | Excellent | Yes      | `isomp4`     | No      | [`symphonia-format-isomp4`] |
| MKV/WebM | Excellent | Yes      | `mkv`        | Yes     | [`symphonia-format-mkv`]    |
| Musepack | Excellent | Yes      | `musepack`   | No      | [`symphonia-bundle-musepack`] |
| MPEG-PS (audio only) | Great | No  | `mpegps`     | No      | [`symphonia-format-mpegps`] |
| MPEG-TS (audio only) | Great | No  | `mpegts`     | No      | [`symphonia-format-mpegts`] |
| OGG      | Excellent | Yes      | `ogg`        | Yes     | [`symphonia-format-ogg`]    |
| Wave     | Excellent | Yes      | `wav`        | Yes     | [`symphonia-format-riff`]   |

\* Gapless playback requires support from both the demuxer and decoder.

\*\* Raw elementary streams have no container timing: their duration is estimated (AC-3, DTS) or unknown (TrueHD/MLP), unless `prebuild_seek_index` is set and the source is seekable, in which case the stream is scanned and the duration is exact.

\*\*\* FLV carries no end padding: only the leading decoder delay (e.g. Opus pre-skip) is trimmed.

[`symphonia-format-caf`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-caf
[`symphonia-format-flv`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-flv
[`symphonia-format-isomp4`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-isomp4
[`symphonia-format-mkv`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-mkv
[`symphonia-format-mpegps`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-mpegps
[`symphonia-format-mpegts`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-mpegts
[`symphonia-format-ogg`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-ogg
[`symphonia-format-riff`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-riff
[`symphonia-format-dsd`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-dsd
[`symphonia-format-es`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-es

> [!TIP]
> All formats can be enabled with the `all-formats` feature flag.

### Codecs (Decoders)

| Codec                        | Status    | Gapless | Feature Flag | Default | Crate                      |
|------------------------------|-----------|---------|--------------|---------|----------------------------|
| AAC-LC                       | Excellent | Yes     | `aac`        | No      | [`symphonia-codec-aac`]    |
| ADPCM                        | Excellent | Yes     | `adpcm`      | Yes     | [`symphonia-codec-adpcm`]  |
| ALAC                         | Excellent | Yes     | `alac`       | No      | [`symphonia-codec-alac`]   |
| APE (Monkey's Audio)         | Excellent | Yes     | `ape-format` | No      | [`symphonia-bundle-ape`]     |
| AC-3, E-AC-3                 | Great     | No      | `ac3`        | No      | [`symphonia-codec-ac3`]      |
| DSD                          | Great     | Yes     | `dsd`        | No      | [`symphonia-codec-dsd`]      |
| DTS (core, XCH, XXCH, X96, XBR, XLL) | Great | No   | `dca`        | No      | [`symphonia-codec-dca`]      |
| FLAC                         | Excellent | Yes     | `flac`       | Yes     | [`symphonia-bundle-flac`]  |
| MP1                          | Excellent | No      | `mp1`, `mpa` | No      | [`symphonia-bundle-mp3`]   |
| MP2                          | Excellent | No      | `mp2`, `mpa` | No      | [`symphonia-bundle-mp3`]   |
| MP3                          | Excellent | Yes     | `mp3`, `mpa` | No      | [`symphonia-bundle-mp3`]   |
| mp3PRO                       | Great     | No      | `mp3pro`     | No      | [`symphonia-bundle-mp3`]   |
| Musepack                     | Excellent | Yes     | `musepack`   | No      | [`symphonia-bundle-musepack`]|
| Nellymoser                   | Excellent | No      | `nellymoser` | No      | [`symphonia-codec-nellymoser`]|
| Opus                         | Excellent | Yes     | `opus`       | Yes     | [`symphonia-codec-opus`]   |
| PCM                          | Excellent | Yes     | `pcm`        | Yes     | [`symphonia-codec-pcm`]    |
| Speex                        | Excellent | Yes     | `speex`      | No      | [`symphonia-codec-speex`]    |
| TrueHD, MLP                  | Excellent | No      | `truehd`     | No      | [`symphonia-codec-truehd`]   |
| Vorbis                       | Excellent | Yes     | `vorbis`     | Yes     | [`symphonia-codec-vorbis`] |
| WavPack                      | Great     | Yes     | `wavpack`    | No      | [`symphonia-codec-wavpack`]  |

A `symphonia-bundle-*` package is a combination of a decoder and a native demuxer.

mp3PRO streams decode as plain MP3 by default. The `mp3pro` feature adds `Mp3ProDecoder`, which is not registered in the default codec registry: register it in place of `MpaDecoder` to decode the SBR high band and get output at twice the core sample rate.

[`symphonia-codec-aac`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-aac
[`symphonia-codec-adpcm`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-adpcm
[`symphonia-codec-alac`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-alac
[`symphonia-bundle-flac`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-bundle-flac
[`symphonia-bundle-mp3`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-bundle-mp3
[`symphonia-codec-pcm`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-pcm
[`symphonia-codec-vorbis`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-vorbis
[`symphonia-codec-opus`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-opus
[`symphonia-bundle-ape`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-bundle-ape
[`symphonia-bundle-musepack`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-bundle-musepack
[`symphonia-codec-ac3`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-ac3
[`symphonia-codec-dca`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-dca
[`symphonia-codec-dsd`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-dsd
[`symphonia-codec-nellymoser`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-nellymoser
[`symphonia-codec-speex`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-speex
[`symphonia-codec-truehd`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-truehd
[`symphonia-codec-wavpack`]: https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-wavpack

> [!TIP]
> All codecs can be enabled with the `all-codecs` feature flag. Similarly, all MPEG audio codecs can be enabled with the `mpa` feature flag.

### Tags (Readers)

All metadata readers are provided by the `symphonia-metadata` crate.

For metadata formats that are standalone and not part of the media container, a feature flag may be used to toggle support.

| Format                | Status    | Feature Flag | Default |
|-----------------------|-----------|--------------|---------|
| APEv1                 | Excellent | `ape`        | Yes     |
| APEv2                 | Excellent | `ape`        | Yes     |
| ID3v1                 | Excellent | `id3v1`      | Yes     |
| ID3v2                 | Excellent | `id3v2`      | Yes     |
| ISO/MP4               | Excellent | N/A          | N/A     |
| RIFF                  | Great     | N/A          | N/A     |
| Vorbis comment (FLAC) | Excellent | N/A          | N/A     |
| Vorbis comment (OGG)  | Excellent | N/A          | N/A     |

> [!TIP]
> All metadata formats can be enabled with the `all-meta` feature flag.

### Known Unsupported Codecs and Containers

Codecs and containers without a row in the tables above are not implemented. Known gaps:

* Video and subtitle codecs: no video or subtitle decoder exists in this repository.
* ATRAC, WMA and RealAudio Cook: codec IDs are reserved in `symphonia-core` (`CODEC_ID_ATRAC*`, `CODEC_ID_WMA`, `CODEC_ID_COOK`), but no decoder crate implements them.
* ASF/WMA, RealMedia and AVI containers: no demuxer crate exists.
* AAC: error resilience tools (Huffman codeword reordering, RVLC, virtual codebooks), LTP in AAC-LD, SBR with AAC-LD, and Parametric Stereo with anything other than a single mono `SCE` core element are rejected as unsupported (see `symphonia-codec-aac`).
* mp3PRO SBR decoding is not in the default codec registry (opt-in via the `mp3pro` feature).

Please open an issue to request a missing format.

### Third-party Demuxers and Decoders

Symphonia is a designed around a modular framework that allows the registration of third-party demuxers and decoders. This table provides a list of crates that implement such components.

> [!IMPORTANT]
> This is not an endorsement of the quality, safety, or performance of these implementations. Please exercise your own judgement.

| Crate                         | Provides                                     |
|-------------------------------|----------------------------------------------|
| [`symphonia-adapter-fdk-aac`] | AAC-LC, HE-AAC, & HE-AACv2 via. `libfdk-aac` |
| [`symphonia-adapter-libopus`] | Opus via. `libopus`                          |

[`symphonia-adapter-fdk-aac`]: https://crates.io/crates/symphonia-adapter-fdk-aac
[`symphonia-adapter-libopus`]: https://crates.io/crates/symphonia-adapter-libopus

## Quality

In addition to the safety guarantees afforded by Rust, Symphonia aims to:

* Decode media as correctly as the leading free-and-open-source software decoders
* Prevent denial-of-service attacks
* Be fuzz-tested
* Provide a powerful, consistent, and easy to use API

## Performance

Symphonia aims to be comparable to, or faster than, popular open-source C-based implementations. Currently, Symphonia's decoders are generally +/-15% the performance of FFMpeg. However, the exact range will depend strongly on the codec, which features of the codec are being leveraged in the encoding, the Rust compiler version, and the CPU architecture being compiled for.

See the [benchmarks](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/BENCHMARKS.md) for more information.

### Optimizations

All SIMD optimizations are enabled by default. SIMD support will pull in the `rustfft` dependency.

| Instruction Set | Feature Flag    | Default |
|-----------------|-----------------|---------|
| SSE             | `opt-simd-sse`  | Yes     |
| AVX             | `opt-simd-avx`  | Yes     |
| Neon            | `opt-simd-neon` | Yes     |

> [!TIP]
> All SIMD optimizations can be enabled with the `opt-simd` feature flag.

## Examples

Basic usage examples may be found [`here`](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia/examples).

For a more complete application, see [`symphonia-play`](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/symphonia-play), a simple music player.

## Tools

Symphonia provides the following tools for debugging purposes:

* [`symphonia-play`](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/symphonia-play) for probing, decoding, validating, and playing back media streams.
* [`symphonia-check`](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/symphonia-check) for validating Symphonia's decoded output against various decoders.

## Minimum Supported Rust Version

The minimum supported Rust version (MSRV) for Symphonia 0.6.x releases is 1.85.

This project considers incrementing the MSRV to be a SemVer breaking change.

## Author

The primary author is Philip Deljanov.

## Special Thanks

* Kostya Shishkov (AAC-LC decoder contribution, see `symphonia-codec-aac`)

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](CONTRIBUTING.md).
