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

- Decoder: Dolby AC-4 (`symphonia-codec-ac4`, feature `ac4`): ASF, A-SPX, A-CPL-less stereo, 5.0 and
  5.1 streams, in MPEG-TS (registration or DVB descriptor), MP4 (`ac-4`/`dac4`), Matroska (`A_AC4`)
  and as sync frames. Matches FFmpeg's `ac4` decoder at about 131 dB SNR.
- Decoders: AC-3 and E-AC-3 (`symphonia-codec-ac3`), DTS including DTS-HD Master Audio
  and DTS Express (LBR) (`symphonia-codec-dca`), Dolby TrueHD and MLP (`symphonia-codec-truehd`), Speex
  (`symphonia-codec-speex`), Nellymoser Asao (`symphonia-codec-nellymoser`), mp3PRO
  (`symphonia-bundle-mp3`, feature `mp3pro`), DSD and DST (`symphonia-codec-dsd`), and a native
  Opus decoder with a CELT-only encoder (`symphonia-codec-opus`).
- Decoder: QDesign Music Codec 1 and 2 (`symphonia-codec-qdesign`, feature `qdesign`), for
  QuickTime/MP4 tracks (`QDMC`/`QDM2`, including FFmpeg-remuxed `glbl` extra data).
- Game console audio (`symphonia-format-game`, `symphonia-codec-adpcm`): Nintendo GameCube DTK
  (`.adp`), AFC (`.afc`) and AST (`.ast`), Sony PlayStation VAG, SVAG, VPK, ADS/SS2, XVAG and MSF
  and BRSTM, BFSTM and BCSTM (features `adp`, `afc`, `ast`, `vag`, `brstm`), with the Nintendo DSP, DTK, AFC and PlayStation ADPCM
  decoders. Synthetic streams decode bit for bit like FFmpeg.
- Decoder and demuxer: CRI HCA (`symphonia-codec-hca`, `symphonia-format-game`, feature `hca`),
  matching FFmpeg's decoder at 139 dB SNR; the muted samples and loop points are not applied.
- Decoder: GSM 06.10 full-rate and Microsoft GSM 6.10 (`symphonia-codec-gsm`, feature `gsm`),
  mapped from WAVE `0x0031`/`0x0032`/`0x1500`, AIFF-C `GSM `, CAF `agsm`/`ms\0\x31`, QuickTime
  `agsm`/`ms\0\x31` and Matroska `A_MS/ACM`.
- RealAudio: RealMedia/RealAudio demuxer (`symphonia-format-real`, feature `real`, for `.rm`, `.rmvb`
  and `.ra`, with the `genr`, `Int4` and `sipr` deinterleavers), and decoders for
  Cook (`symphonia-codec-cook`, `cook`), RealAudio 1.0/2.0 (`symphonia-codec-ra`, `ra`),
  RealAudio Lossless (`symphonia-codec-ralf`, `ralf`) and SIPR in all four modes
  (`symphonia-codec-sipr`, `sipr`), checked against FFmpeg.
- Decoders: ATRAC1 and ATRAC3 (`symphonia-codec-atrac`, feature `atrac`), ATRAC3+
  (`symphonia-codec-atrac3plus`, feature `atrac3plus`, from WAVE/OMA frames), ATRAC9
  (`symphonia-codec-atrac9`, feature `atrac9`, from `.at9` RIFF/WAVE) and MPEG-4 ALS
  (`symphonia-codec-als`, feature `als`, including the floating-point and multi-channel modes),
  checked against FFmpeg.
- Demuxers: Sun/NeXT AU, Creative VOC, IFF 8SVX/16SV/MAUD and headerless PCM
  (`symphonia-format-sound`, features `au`, `voc`, `iff`, `raw-pcm`) and CRI ADX
  (`symphonia-format-game`, feature `adx`).
- Demuxer: AVI audio streams (PCM, float, A-law, mu-law, MS and IMA ADPCM, MPEG audio layers
  1/2/3, OpenDML continuations; `symphonia-format-avi`, feature `avi`).
- Bundles: TTA (`symphonia-bundle-tta`, feature `tta`), TAK (`symphonia-bundle-tak`, feature `tak`)
  and Shorten (`symphonia-bundle-shorten`, feature `shorten`): decoder and demuxer each,
  bit-exact with FFmpeg.
- Decoders: SBC and mSBC (`symphonia-codec-sbc`, feature `sbc`), aptX and aptX HD
  (`symphonia-codec-aptx`, feature `aptx`), LC3 and LC3plus including high resolution
  (`symphonia-codec-lc3`, feature `lc3`) and Dolby E (`symphonia-codec-dolby-e`, feature
  (`symphonia-codec-lc3`, feature `lc3`), LDAC (`symphonia-codec-ldac`, feature `ldac`) and
  Dolby E (`symphonia-codec-dolby-e`, feature
  `dolby-e`), with the raw SBC/aptX/aptX HD streams and `.lc3` files (`symphonia-format-bt`,
  feature `bt`), IEC 61937 data bursts (`symphonia-format-spdif`, feature `spdif`) and SMPTE ST 337
  Dolby E streams (`symphonia-format-es`).
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
- MPEG audio (MP1/MP2/MP3): a mid-stream change of the signal specification (sample rate or
  channel count, as in concatenated files) reports `ResetRequired` and restarts the new stream
  from a clean state instead of failing.
- mp3PRO: gapless playback (the LAME delay/padding and the 962-sample SBR delay are removed), and
  with the `mp3pro` feature the SBR decoder is registered in the default codec registry in place
  of `MpaDecoder` for MP3.

### Fixed

- Upstream issues that still reproduced on this tree, FLAC frame CRC and LPC shift validation,
  Vorbis with more than 8 channels, ALAC order-31 prediction, and many malformed-input panics.

[Unreleased]: https://github.com/M0Rf30/Symphonia/compare/v0.6.1...rmpd-next
