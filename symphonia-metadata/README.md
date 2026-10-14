# Symphonia Metadata Utilities

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_metadata-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-metadata)

Project Symphonia metadata format readers and helper utilities.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Formats and features

| Format                | Feature flag  | Default | Module                                  |
|-----------------------|---------------|---------|-----------------------------------------|
| APEv1, APEv2          | `ape`         | Yes     | `ape`                                   |
| ID3v1                 | `id3v1`       | Yes     | `id3v1`                                 |
| ID3v2                 | `id3v2`       | Yes     | `id3v2`                                 |
| FLAC metadata blocks  | `flac`        | No      | `embedded::flac` (also enables `vorbis`)|
| Vorbis comment        | `vorbis`      | No      | `embedded::vorbis` (also enables `flac`)|
| RIFF INFO             | `riff-info`   | No      | `embedded::riff`                        |
| RIFF ID3              | `riff-id3`    | No      | `embedded::riff` (also enables `id3v2`) |

`riff` enables both RIFF readers. The `write` feature enables tag serializers (used by
`symphonia-tagging`). Embedded readers are normally enabled by the container crates that use them.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
