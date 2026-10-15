# Symphonia IEC 61937 (S/PDIF) demuxer

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_format_spdif-brightgreen?style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-spdif)

IEC 61937 data burst demuxer for Project Symphonia: compressed audio (AC-3, E-AC-3, DTS, MPEG audio
layers 1-3, AAC in ADTS) carried in S/PDIF data bursts, as written by `ffmpeg -f spdif`.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.
The burst framing is derived from FFmpeg (LGPL-2.1-or-later), see `NOTICE`.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
