# Symphonia TAK Codec

[![Docs](https://docs.rs/symphonia-bundle-tak/badge.svg)](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-bundle-tak)

TAK (Tom's lossless Audio Kompressor) demuxer and decoder for Project Symphonia.

Supports TAK 2.x streams (the version FFmpeg decodes) with 8, 16 and 24 bit samples, one to six
channels, all presets and frame sizes, seeking, and the stream MD5 (as a verification code).

**Note:** This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details. The decoder follows FFmpeg's LGPL-2.1+ TAK decoder; see the NOTICE file.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/CONTRIBUTING.md).
