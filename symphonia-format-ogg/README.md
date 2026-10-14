# Symphonia OGG Demuxer

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_format_ogg-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-format-ogg)

OGG demuxer for Project Symphonia.

Mapped codecs: Vorbis, Opus, FLAC, Speex, CELT (the legacy 0.x header; demuxed only, there is no
CELT decoder), and PCM (OggPCM draft: integer, float, A-law and mu-law formats).

With the `writer` cargo feature the crate also provides a minimal, codec-agnostic Ogg page
writer (`symphonia_format_ogg::writer`, RFC 3533): lacing, page splitting, granule positions,
BOS/EOS flags and page CRC.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
