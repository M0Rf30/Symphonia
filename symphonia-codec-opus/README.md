# Symphonia Opus Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_opus-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-opus)

A pure-Rust, native Opus (RFC 6716) decoder for Project Symphonia. This is a from-scratch port
of the reference `libopus` decoder algorithms into safe Rust (`#![forbid(unsafe_code)]`), not a
wrapper/binding around `libopus` or any other native library. See `NOTICE` for the provenance and
license of the ported algorithms (BSD-3-Clause), which coexists with this crate's own MPL-2.0
license.

## Status

* All three Opus coding modes: SILK (speech), CELT (music/low-latency), and Hybrid.
* Multistream/multichannel decoding (`OpusMultistream`), including surround mappings
  (e.g. 5.1) via RFC 7845 channel-order scatter.
* Packet loss concealment (PLC) and forward error correction (FEC) redundancy decoding.
* Gapless playback support (RFC 7845 `pre_skip`/Matroska `CodecDelay`, handled by the format
  readers in `symphonia-format-ogg`/`symphonia-format-mkv`) and seek pre-roll (RFC 7845 section
  4.6 / Matroska `SeekPreRoll`).
* Conformant against all 12 RFC 8251 test vectors (final range coder state and PCM output,
  mono and stereo) and cross-checked against `libopus` on real-world Ogg/WebM content (SILK
  bit-exact; CELT/Hybrid within audible-transparency SNR of `libopus`).

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/blob/main/CONTRIBUTING.md).
