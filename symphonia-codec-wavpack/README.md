# Symphonia WavPack Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_wavpack-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-wavpack)

WavPack demuxer and lossless decoder for Project Symphonia.

Supports the WavPack v1-v3 RIFF/WAVE-wrapped stream and the native WavPack v4/v5 block
stream: lossless mono/stereo PCM (8/16/24/32-bit), lossless and hybrid-lossy IEEE 32-bit
float, and hybrid-lossy PCM. Multichannel (>2 channel) WavPack files and the `.wvc`
correction-file mechanism (needed for exact lossless reconstruction of a hybrid stream)
are not supported; see below.

### Hybrid (`-b<n>`) streams and `.wvc`

WavPack's hybrid mode splits a stream into a "lossy" main bitstream plus an optional
`.wvc` correction file that refines it back to bit-exact lossless. This decoder always
decodes the lossy approximation from the main bitstream alone (bit-exact vs. `wvunpack
-i`, which forces the same "ignore .wvc" behaviour), even when a sibling `.wvc` file
exists next to the `.wv` file being played.

This is a hard limitation of the current Symphonia API rather than a decoding gap:
`symphonia_core::formats::FormatOptions::external_data` (`ExternalFormatData`) only
carries pre-parsed metadata/chapters, not an extra `MediaSourceStream`, so a
`FormatReader::try_new` has no side channel through which a caller could hand it the
correction file. Supporting `.wvc` would need a new `ExternalFormatData` field (e.g. an
optional boxed `MediaSourceStream`/reader for the correction file) plumbed through the
probe/format-registration API; that is out of scope for this crate alone. A caller (e.g.
rmpd) that wants lossless hybrid playback today has no way to supply the `.wvc` stream to
this reader.

IEEE float and hybrid-lossy int/float decoding were ported from the reference WavPack
library (BSD-3-Clause, see `LICENSE`/`NOTICE`); the ported functions are documented at
each call site with the corresponding upstream C source file.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/blob/main/CONTRIBUTING.md).
