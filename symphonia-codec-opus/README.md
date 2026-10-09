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
  (e.g. 5.1) via RFC 7845 channel-order scatter, and ambisonics with a demixing matrix
  (mapping family 3, RFC 8486, libopus' projection decoder).
* Packet loss concealment (PLC) and forward error correction (FEC) redundancy decoding.
* Gapless playback support (RFC 7845 `pre_skip`/Matroska `CodecDelay`, handled by the format
  readers in `symphonia-format-ogg`/`symphonia-format-mkv`) and seek pre-roll: 1.5 s for CELT-only
  streams (bit-identical to a continuous decode afterwards), 10 s for streams with SILK/Hybrid
  packets (SILK's fixed-point state may never fully converge after a seek, as in `libopus`
  itself); longer than the 80 ms of RFC 7845 section 4.6 / Matroska `SeekPreRoll`.
* Conformant against all 12 RFC 8251 test vectors (final range coder state and PCM output,
  mono and stereo) and cross-checked against `libopus` on real-world Ogg/WebM content (SILK
  bit-exact; CELT/Hybrid within audible-transparency SNR of `libopus`).

## Encoder (optional)

With the `encoder` cargo feature the crate also contains a pure-Rust Opus **encoder**
(`symphonia_codec_opus::encoder`), a port of the CELT half of `libopus` specialised for music
streaming: CELT-only mode, 48 kHz, mono/stereo, 20 ms frames, CBR / VBR / constrained VBR at a
configurable bitrate, a complexity knob (0-10), MDCT with transient detection and short blocks,
TF analysis, spreading decision, dynamic allocation, intensity/dual/mid-side stereo, and the
RFC 7845 `OpusHead`/`OpusTags` header builders. The `ogg` feature adds an Ogg Opus muxer
(`encoder::ogg::OggOpusWriter`) on top of the page writer in `symphonia-format-ogg` (feature
`writer`). The pitch pre-filter, SILK/hybrid modes and QEXT are not implemented; this affects
compression efficiency only. The streams decode identically in this crate's decoder, `libopus`
and FFmpeg, and reach the same SNR as `libopus` at equal bitrate on the test material.

```toml
symphonia-codec-opus = { version = "0.6", features = ["ogg"] }
```

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/blob/main/CONTRIBUTING.md).
