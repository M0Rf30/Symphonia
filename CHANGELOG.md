# Changelog

All notable changes to this fork (`M0Rf30/Symphonia`, branch `rmpd-next`) are recorded here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/). Per-commit detail is in `git log`; per-codec status is
in [README.md](README.md).

Releases are cut by pushing a `vX.Y.Z` tag: `.github/workflows/release.yml` checks that the tag
matches the workspace version, uses the matching section of this file as the release notes, and
attaches `symphonia-play` binaries. Move the `Unreleased` entries under a new version heading (and
bump `[workspace.package] version` in `Cargo.toml`) before tagging.

## [Unreleased]

Changes since upstream `v0.6.1`.

### Added

- Decoders: AC-3 and E-AC-3 (`symphonia-codec-ac3`), DTS including DTS-HD Master Audio
  and DTS Express (LBR) (`symphonia-codec-dca`), Dolby TrueHD and MLP (`symphonia-codec-truehd`), Speex
  (`symphonia-codec-speex`), Nellymoser Asao (`symphonia-codec-nellymoser`), mp3PRO
  (`symphonia-bundle-mp3`, feature `mp3pro`), DSD and DST (`symphonia-codec-dsd`), and a native
  Opus decoder with a CELT-only encoder (`symphonia-codec-opus`).
- Decoders: SBC and mSBC (`symphonia-codec-sbc`, feature `sbc`), aptX and aptX HD
  (`symphonia-codec-aptx`, feature `aptx`), LC3 and LC3plus including high resolution
  (`symphonia-codec-lc3`, feature `lc3`) and Dolby E (`symphonia-codec-dolby-e`, feature
  `dolby-e`), with the raw SBC/aptX/aptX HD streams and `.lc3` files (`symphonia-format-bt`,
  feature `bt`), IEC 61937 data bursts (`symphonia-format-spdif`, feature `spdif`) and SMPTE ST 337
  Dolby E streams (`symphonia-format-es`).
- Decoder: GSM 06.10 full-rate and Microsoft GSM 6.10 (`symphonia-codec-gsm`, feature `gsm`),
- Decoder: GSM 06.10 full-rate and Microsoft GSM 6.10 (`symphonia-codec-gsm`, feature `gsm`),
  mapped from WAVE `0x0031`/`0x0032`/`0x1500`, AIFF-C `GSM `, CAF `agsm`/`ms\0\x31`, QuickTime
  `agsm`/`ms\0\x31` and Matroska `A_MS/ACM`.
- Decoder: Windows Media Audio 1 and 2 (`symphonia-codec-wma`, feature `wma`), and an ASF/WMA
  demuxer (`symphonia-format-asf`, feature `asf`) with seeking and tags; WAVE `0x0160` to `0x0163`
  are mapped to the WMA codec IDs.
- Bundles: Musepack SV4 to SV8 (`symphonia-bundle-musepack`); Monkey's Audio legacy versions and
  multichannel 32-bit streams (`symphonia-bundle-ape`).
- AAC: SBR, parametric stereo, coupling channels, pulse data, AAC-LD and AAC-ELD with low delay
  SBR, implicit SBR/PS detection, ADIF reader.
- WavPack: DSD, IEEE float, hybrid lossy and hybrid lossless with `.wvc` correction files.
- Containers: audio-only FLV, MPEG program stream and MPEG transport stream demuxers, raw
  elementary streams (`symphonia-format-es`), DSF/DFF (`symphonia-format-dsd`).
- ADPCM: bit-exact IMA, Yamaha, SWF, G.722 and G.726.
- Tag writing (`symphonia-tagging`) for ID3v1/v2, APEv2, Vorbis comments, RIFF INFO, MP4 `ilst`,
  FLAC, Ogg, WAV, AIFF and DSF, with the serializers in `symphonia-metadata`.
- `symphonia-core` DSP: a streaming windowed-sinc polyphase sample-rate converter
  (`dsp::resample`) and ITU-R BS.775 channel downmix matrices (`dsp::downmix`).
- `symphonia/examples/decode_bench.rs`: decode timing and bit-exactness fingerprints.
- Fuzz targets for AC-3, DTS, TrueHD, Speex, Nellymoser and elementary streams.
- CI: commit-by-commit build check, MSRV tests, coverage report, benchmark job, big-endian
  (PowerPC) tests, `symphonia-play` clippy/test/doc coverage, and a tag-driven release workflow.

### Changed

- Faster decoding in APE, WavPack, Musepack, DSD, FLAC, Opus and AAC SBR.
- The probe scans zero padding quickly and skips metadata that fails to read; ID3v2 tags are read
  from appended positions in every container.
- Gapless handling and seeking improved in MP4, MKV, Ogg, CAF, RIFF and MPEG-in-WAVE.

### Fixed

- Upstream issues that still reproduced on this tree, FLAC frame CRC and LPC shift validation,
  Vorbis with more than 8 channels, ALAC order-31 prediction, and many malformed-input panics.

[Unreleased]: https://github.com/M0Rf30/Symphonia/compare/v0.6.1...rmpd-next
