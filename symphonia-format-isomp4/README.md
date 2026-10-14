# Symphonia ISO/MP4 Demuxer

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_format_isomp4-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-format-isomp4)

ISO/MP4 demuxer for Project Symphonia.

> [!NOTE]
> This crate is part of Project Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Features

* `writer`: exposes `symphonia_format_isomp4::writer`, the serializer that turns tags and pictures back into an `ilst` atom (the reverse of the reader's mapping), used by `symphonia-tagging` to write MP4/M4A tags.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
