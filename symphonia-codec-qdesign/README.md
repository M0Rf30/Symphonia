# Symphonia QDesign Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_qdesign-brightgreen?style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-qdesign)

QDesign Music Codec (QDM2 and QDMC) decoder for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Supported

- QDM2 and QDMC (QDesign Music Codec 1) streams, as stored in MP4/MOV and Matroska files.

The codec's extra data is the QuickTime sound sample description extension containing the `frma`
and `QDCA` atoms. The decoders are ports of FFmpeg's `qdm2.c` and `qdmc.c`; see the NOTICE file.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more
details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please
read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
